//! Dependency resolution: from requested ranges to an exact package set.
//!
//! npm-flavored semver matching is delegated to the `nodejs_semver` crate
//! (the node-semver port used by orogene). For each `(name, range)`
//! requirement we pick the highest published version satisfying the range,
//! preferring a version already pinned in the lockfile when it still
//! satisfies — so `chidori add foo` doesn't churn unrelated pins.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use anyhow::{bail, Context, Result};
use nodejs_semver::{Range, Version};

use super::registry::{PackageVersion, RegistryClient};

/// An exact package selected by resolution.
#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub name: String,
    pub version: String,
    pub tarball: String,
    pub integrity: Option<String>,
    pub shasum: Option<String>,
    /// Resolved dependency edges: dep name -> exact version chosen.
    pub dependencies: BTreeMap<String, String>,
}

impl ResolvedPackage {
    pub fn id(&self) -> (String, String) {
        (self.name.clone(), self.version.clone())
    }
}

/// The full resolved set plus the root's direct edges.
#[derive(Debug, Default)]
pub struct Resolution {
    /// All packages, keyed by (name, exact version).
    pub packages: BTreeMap<(String, String), ResolvedPackage>,
    /// Root direct dependencies (prod + dev merged): name -> exact version.
    pub roots: BTreeMap<String, String>,
    /// Human-readable warnings (unsatisfied peers, deprecations, skipped
    /// optionals) to surface after install.
    pub warnings: Vec<String>,
}

/// Dependency spec forms the registry cannot serve (`file:`, `git:`,
/// `workspace:`, …). Returns the human-readable kind when `spec` is one.
///
/// Callers decide the policy: manifest-level deps in these forms are
/// *skipped per-dependency* with a warning (one `file:` line must not brick
/// `add`/`install`/`remove` for the whole project), while an explicitly
/// requested `chidori add name@file:…` is still a hard error.
pub fn unsupported_spec_kind(spec: &str) -> Option<&'static str> {
    let spec = spec.trim();
    for (prefix, kind) in [
        ("git+", "git"),
        ("git:", "git"),
        ("github:", "git"),
        ("file:", "file"),
        ("link:", "link"),
        ("workspace:", "workspace"),
        ("npm:", "alias"),
        ("http://", "url"),
        ("https://", "url"),
    ] {
        if spec.starts_with(prefix) {
            return Some(kind);
        }
    }
    None
}

/// Pick the version of `name` that satisfies `spec` from a packument.
///
/// `spec` may be a semver range (`^1.2.3`, `1.x`, `>=2 <3 || 4.x`, …) or a
/// dist-tag (`latest`, `beta`). `preferred` versions win when they satisfy.
fn select_version<'p>(
    name: &str,
    spec: &str,
    packument: &'p super::registry::Packument,
    preferred: &BTreeSet<String>,
) -> Result<&'p PackageVersion> {
    let spec = spec.trim();
    if let Some(kind) = unsupported_spec_kind(spec) {
        bail!(
            "`{name}@{spec}`: {kind} dependencies are not supported by chidori's package manager"
        );
    }

    let range: Option<Range> = if spec.is_empty() {
        Some("*".parse().expect("wildcard range parses"))
    } else {
        spec.parse().ok()
    };

    let matching_version = |range: &Range| -> Option<&'p PackageVersion> {
        // Preferred (lockfile-pinned) versions first, then highest overall.
        let mut best: Option<(Version, &PackageVersion)> = None;
        let mut best_preferred: Option<(Version, &PackageVersion)> = None;
        for (raw, meta) in &packument.versions {
            let Ok(version) = raw.parse::<Version>() else {
                continue;
            };
            if !version.satisfies(range) {
                continue;
            }
            if preferred.contains(raw) && best_preferred.as_ref().is_none_or(|(b, _)| version > *b)
            {
                best_preferred = Some((version.clone(), meta));
            }
            if best.as_ref().is_none_or(|(b, _)| version > *b) {
                best = Some((version, meta));
            }
        }
        best_preferred.or(best).map(|(_, meta)| meta)
    };

    if let Some(range) = range {
        if let Some(meta) = matching_version(&range) {
            return Ok(meta);
        }
        bail!(
            "no version of `{name}` satisfies `{spec}` (available: {} versions)",
            packument.versions.len()
        );
    }

    // Not a parsable range: try it as a dist-tag.
    if let Some(tagged) = packument.dist_tags.get(spec) {
        if let Some(meta) = packument.versions.get(tagged) {
            return Ok(meta);
        }
        bail!("dist-tag `{spec}` of `{name}` points at unpublished version {tagged}");
    }
    bail!("`{name}@{spec}` is neither a valid semver range nor a dist-tag");
}

/// Resolve `root_deps` (name -> range) plus their full transitive closure.
///
/// `preferred` seeds version choices from an existing lockfile: for each
/// name, versions we'd like to keep if they still satisfy the range asking.
pub async fn resolve(
    registry: &RegistryClient,
    root_deps: &BTreeMap<String, String>,
    preferred: &HashMap<String, BTreeSet<String>>,
) -> Result<Resolution> {
    let empty = BTreeSet::new();
    let mut resolution = Resolution::default();
    // Memoized picks so one (name, range) pair resolves identically everywhere.
    let mut picked: HashMap<(String, String), (String, String)> = HashMap::new();
    // Queue of (name, range, optional, requested_by).
    let mut queue: VecDeque<(String, String, bool, String)> = root_deps
        .iter()
        .map(|(n, r)| (n.clone(), r.clone(), false, "the project".to_string()))
        .collect();
    let mut root_pending: BTreeMap<String, String> = root_deps.clone();

    while let Some((name, range, optional, requested_by)) = queue.pop_front() {
        let key = (name.clone(), range.clone());
        let already = picked.get(&key).cloned();
        let meta = match already {
            Some((n, v)) => {
                if root_pending.remove(&name).is_some() {
                    resolution.roots.insert(n, v);
                }
                continue;
            }
            None => {
                let packument = match registry.packument(&name).await {
                    Ok(p) => p,
                    Err(e) if optional => {
                        resolution
                            .warnings
                            .push(format!("skipped optional dependency `{name}`: {e}"));
                        continue;
                    }
                    Err(e) => {
                        return Err(e).with_context(|| {
                            format!("resolving `{name}@{range}` (required by {requested_by})")
                        })
                    }
                };
                let selected = select_version(
                    &name,
                    &range,
                    &packument,
                    preferred.get(&name).unwrap_or(&empty),
                );
                match selected {
                    Ok(meta) => meta.clone(),
                    Err(e) if optional => {
                        resolution
                            .warnings
                            .push(format!("skipped optional dependency `{name}@{range}`: {e}"));
                        continue;
                    }
                    Err(e) => {
                        return Err(e).with_context(|| {
                            format!("resolving `{name}@{range}` (required by {requested_by})")
                        })
                    }
                }
            }
        };

        picked.insert(key, (meta.name.clone(), meta.version.clone()));
        if root_pending.remove(&name).is_some() {
            resolution
                .roots
                .insert(meta.name.clone(), meta.version.clone());
        }
        if let Some(msg) = &meta.deprecated {
            if !msg.is_empty() {
                resolution.warnings.push(format!(
                    "`{}@{}` is deprecated: {msg}",
                    meta.name, meta.version
                ));
            }
        }

        let id = (meta.name.clone(), meta.version.clone());
        if resolution.packages.contains_key(&id) {
            continue;
        }

        let requester = format!("{}@{}", meta.name, meta.version);
        for (dep, dep_range) in &meta.dependencies {
            queue.push_back((dep.clone(), dep_range.clone(), false, requester.clone()));
        }
        for (dep, dep_range) in &meta.optional_dependencies {
            queue.push_back((dep.clone(), dep_range.clone(), true, requester.clone()));
        }

        resolution.packages.insert(
            id,
            ResolvedPackage {
                name: meta.name.clone(),
                version: meta.version.clone(),
                tarball: meta.dist.tarball.clone(),
                integrity: meta.dist.integrity.clone(),
                shasum: meta.dist.shasum.clone(),
                // Dependency edges get their exact versions filled in below,
                // once every (name, range) pick is known.
                dependencies: BTreeMap::new(),
            },
        );
    }

    // Second pass: turn each package's (name -> range) requirements into
    // (name -> exact version) edges using the memoized picks.
    let metas: Vec<(String, String)> = resolution.packages.keys().cloned().collect();
    for id in metas {
        let packument = registry.packument(&id.0).await?;
        let Some(meta) = packument.versions.get(&id.1) else {
            continue;
        };
        let mut edges = BTreeMap::new();
        for (dep, dep_range) in &meta.dependencies {
            if let Some((n, v)) = picked.get(&(dep.clone(), dep_range.clone())) {
                edges.insert(n.clone(), v.clone());
            }
        }
        for (dep, dep_range) in &meta.optional_dependencies {
            if let Some((n, v)) = picked.get(&(dep.clone(), dep_range.clone())) {
                edges.insert(n.clone(), v.clone());
            }
        }
        // Peer dependency check: warn when nothing in the resolved set
        // satisfies a peer range. We don't auto-install peers (v1).
        for (peer, peer_range) in &meta.peer_dependencies {
            // Peers marked optional in `peerDependenciesMeta` are explicitly
            // fine to leave uninstalled — never warn about them.
            if meta
                .peer_dependencies_meta
                .get(peer)
                .is_some_and(|m| m.optional)
            {
                continue;
            }
            // The chidori runtime transpiles and executes TypeScript itself,
            // so a `typescript` peer (declared by e.g. valibot) is always
            // effectively satisfied; warning about it would be noise on
            // every install.
            if peer == "typescript" {
                continue;
            }
            let satisfied = resolution
                .packages
                .keys()
                .filter(|(n, _)| n == peer)
                .any(
                    |(_, v)| match (v.parse::<Version>(), peer_range.parse::<Range>()) {
                        (Ok(ver), Ok(range)) => ver.satisfies(&range),
                        _ => true,
                    },
                );
            if !satisfied {
                resolution.warnings.push(format!(
                    "`{}@{}` has unmet peer dependency `{peer}@{peer_range}` (install it explicitly if needed)",
                    id.0, id.1
                ));
            }
        }
        resolution
            .packages
            .get_mut(&id)
            .expect("id came from packages")
            .dependencies = edges;
    }

    Ok(resolution)
}
