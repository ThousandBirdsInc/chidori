//! Command orchestration for `chidori add` / `install` / `remove`.
//!
//! All three commands share one pipeline: produce a `Resolution` (from the
//! registry or the lockfile), plan the hoisted layout, make sure every
//! package is in the content-addressed store (downloading + verifying only
//! misses), hardlink the plan into `node_modules`, and prune anything the
//! plan doesn't claim.
//!
//! `remove` and an in-sync `install` never touch the network: the lockfile
//! carries enough (exact versions, edges, tarball URLs, integrity) to rebuild
//! the tree from the store alone.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use futures::StreamExt as _;

use super::layout::{chain_to_path, plan_layout, LayoutPlan};
use super::lockfile::{Lockfile, LOCKFILE_NAME};
use super::manifest::Manifest;
use super::registry::{validate_package_name, RegistryClient};
use super::resolve::{resolve, unsupported_spec_kind, Resolution};
use super::store::{Integrity, PackageStore};

/// Concurrent tarball downloads. Hashing/extraction runs on the blocking
/// pool, so this only bounds network fan-out.
const DOWNLOAD_CONCURRENCY: usize = 8;

/// Manifest dependencies split into the registry-resolvable set and the
/// forms chidori's package manager cannot manage (`file:`, `git:`, …).
struct RootDeps {
    supported: BTreeMap<String, String>,
    /// (name, spec, kind) of every skipped manifest dependency.
    skipped: Vec<(String, String, &'static str)>,
}

/// Partition manifest deps and warn once per skipped entry. Skipped deps are
/// per-dependency: they stay in package.json untouched, their `node_modules`
/// entries are exempt from pruning, and everything else proceeds — one
/// `file:` line must not block the whole project.
fn partition_root_deps(deps: BTreeMap<String, String>) -> RootDeps {
    let mut supported = BTreeMap::new();
    let mut skipped = Vec::new();
    for (name, spec) in deps {
        match unsupported_spec_kind(&spec) {
            Some(kind) => skipped.push((name, spec, kind)),
            None => {
                supported.insert(name, spec);
            }
        }
    }
    for (name, spec, kind) in &skipped {
        eprintln!(
            "warning: skipping `{name}@{spec}`: {kind} dependencies are not managed by \
             chidori's package manager; package.json and any existing node_modules/{name} \
             are left untouched"
        );
    }
    RootDeps { supported, skipped }
}

/// Top-level `node_modules` names pruning must leave alone: skipped deps may
/// have been materialized by another tool (npm/bun symlinks or copies).
fn keep_names(roots: &RootDeps) -> BTreeSet<String> {
    roots
        .skipped
        .iter()
        .filter(|(name, _, _)| !roots.supported.contains_key(name))
        .map(|(name, _, _)| name.clone())
        .collect()
}

/// `chidori add <spec>... [--dev]`
pub fn cmd_add(dir: &Path, specs: &[String], dev: bool) -> Result<()> {
    if specs.is_empty() {
        bail!("nothing to add: pass one or more packages, e.g. `chidori add zod`");
    }
    let started = Instant::now();
    let mut manifest = Manifest::load_or_default(dir)?;
    let lockfile = load_lockfile_if_present(dir)?;

    let requested: Vec<(String, Option<String>)> = specs
        .iter()
        .map(|s| parse_add_spec(s))
        .collect::<Result<_>>()?;
    // Asking for an unsupported form by name is still a hard error — only
    // *pre-existing* manifest entries are skipped leniently.
    for (name, range) in &requested {
        if let Some(kind) = range.as_deref().and_then(unsupported_spec_kind) {
            bail!(
                "`{name}@{}`: {kind} dependencies are not supported by chidori's package manager",
                range.as_deref().unwrap_or_default()
            );
        }
    }

    // Root set = current manifest deps + the new specs (range or `latest`).
    let mut roots = partition_root_deps(manifest.all_dependencies());
    for (name, range) in &requested {
        roots.supported.insert(
            name.clone(),
            range.clone().unwrap_or_else(|| "latest".to_string()),
        );
    }
    let root_deps = roots.supported.clone();

    let registry = RegistryClient::from_env()?;
    let preferred = preferred_versions(lockfile.as_ref());
    let resolution = block_on(resolve(&registry, &root_deps, &preferred))?;

    // Record what we added: an explicit range verbatim, otherwise a caret
    // range on the resolved version (`^1.2.3`), like npm.
    for (name, range) in &requested {
        let resolved_version = resolution
            .roots
            .get(name)
            .ok_or_else(|| anyhow!("`{name}` missing from resolution"))?;
        let manifest_range = range
            .clone()
            .unwrap_or_else(|| format!("^{resolved_version}"));
        manifest.set_dependency(name, &manifest_range, dev);
    }
    manifest.save()?;

    let lockfile = Lockfile::from_resolution(
        &resolution,
        manifest.dependencies(),
        manifest.dev_dependencies(),
    );
    lockfile.save(&dir.join(LOCKFILE_NAME))?;

    let stats = sync_tree(dir, &resolution, Some(&registry), &keep_names(&roots))?;
    for (name, _) in &requested {
        let version = &resolution.roots[name];
        println!("+ {name}@{version}");
    }
    report(&resolution, &stats, started);

    // The install itself is pure data movement, so an incompatible package
    // (CJS-only, native addon, non-allowlisted builtins) "succeeds" here and
    // only explodes on first import. Surface that now instead.
    for (name, _) in &requested {
        let pkg_dir = dir.join("node_modules").join(name);
        for warning in super::compat::check_package_compat(name, &pkg_dir) {
            eprintln!("warning: {warning}");
        }
    }
    Ok(())
}

/// `chidori install [--frozen]`
pub fn cmd_install(dir: &Path, frozen: bool) -> Result<()> {
    let started = Instant::now();
    let manifest = Manifest::load_or_default(dir)?;
    let lockfile = load_lockfile_if_present(dir)?;
    let deps = manifest.dependencies();
    let dev_deps = manifest.dev_dependencies();

    if deps.is_empty() && dev_deps.is_empty() && lockfile.is_none() {
        println!("nothing to install (no dependencies in package.json)");
        return Ok(());
    }

    let roots = partition_root_deps(manifest.all_dependencies());
    let (resolution, registry) = match &lockfile {
        Some(lock) if lock.matches_manifest(&deps, &dev_deps) => {
            // In sync: no resolution needed; network only for store misses.
            (lock.to_resolution(), None)
        }
        Some(_) if frozen => bail!(
            "{LOCKFILE_NAME} is out of sync with package.json (run `chidori install` without --frozen to update it)"
        ),
        None if frozen => bail!("--frozen requires an existing {LOCKFILE_NAME}"),
        _ => {
            let registry = RegistryClient::from_env()?;
            let preferred = preferred_versions(lockfile.as_ref());
            let resolution = block_on(resolve(&registry, &roots.supported, &preferred))?;
            Lockfile::from_resolution(&resolution, deps, dev_deps)
                .save(&dir.join(LOCKFILE_NAME))?;
            (resolution, Some(registry))
        }
    };

    let stats = sync_tree(dir, &resolution, registry.as_ref(), &keep_names(&roots))?;
    report(&resolution, &stats, started);
    Ok(())
}

/// `chidori remove <name>...`
pub fn cmd_remove(dir: &Path, names: &[String]) -> Result<()> {
    if names.is_empty() {
        bail!("nothing to remove: pass one or more package names");
    }
    let started = Instant::now();
    let mut manifest = Manifest::load_or_default(dir)?;
    for name in names {
        validate_package_name(name)?;
        if !manifest.remove_dependency(name) {
            bail!("`{name}` is not a dependency in package.json");
        }
    }
    manifest.save()?;
    let deps = manifest.dependencies();
    let dev_deps = manifest.dev_dependencies();
    let roots = partition_root_deps(manifest.all_dependencies());

    // Prefer the offline path: shrink the existing lockfile graph to what's
    // still reachable from the remaining roots.
    let lockfile = load_lockfile_if_present(dir)?;
    let resolution = match lockfile {
        Some(lock)
            if names.iter().all(|n| lock.roots.contains_key(n))
                && lock
                    .requested
                    .iter()
                    .chain(lock.requested_dev.iter())
                    .filter(|(n, _)| !names.contains(n))
                    .all(|(n, r)| deps.get(n).or_else(|| dev_deps.get(n)) == Some(r)) =>
        {
            shrink_to_reachable(&lock, names)
        }
        _ => {
            // Lockfile absent or drifted: re-resolve what's left.
            let registry = RegistryClient::from_env()?;
            block_on(resolve(&registry, &roots.supported, &HashMap::new()))?
        }
    };

    Lockfile::from_resolution(&resolution, deps, dev_deps).save(&dir.join(LOCKFILE_NAME))?;
    let stats = sync_tree(dir, &resolution, None, &keep_names(&roots))?;
    for name in names {
        println!("- {name}");
    }
    report(&resolution, &stats, started);
    Ok(())
}

/// Drop removed roots and keep only packages still reachable via lockfile
/// edges. Pure graph walk — no network.
fn shrink_to_reachable(lock: &Lockfile, removed: &[String]) -> Resolution {
    let mut resolution = lock.to_resolution();
    for name in removed {
        resolution.roots.remove(name);
    }
    let mut reachable: BTreeSet<(String, String)> = BTreeSet::new();
    let mut stack: Vec<(String, String)> = resolution
        .roots
        .iter()
        .map(|(n, v)| (n.clone(), v.clone()))
        .collect();
    while let Some(id) = stack.pop() {
        if !reachable.insert(id.clone()) {
            continue;
        }
        if let Some(pkg) = resolution.packages.get(&id) {
            for (n, v) in &pkg.dependencies {
                stack.push((n.clone(), v.clone()));
            }
        }
    }
    resolution.packages.retain(|id, _| reachable.contains(id));
    resolution
}

pub struct SyncStats {
    pub installed: usize,
    pub downloaded: usize,
    pub linked: usize,
    pub pruned: usize,
}

/// Make `node_modules` match the resolution exactly — except `keep`:
/// top-level entries for manifest deps chidori doesn't manage (`file:`,
/// `git:`, …), which pruning leaves alone.
fn sync_tree(
    dir: &Path,
    resolution: &Resolution,
    registry: Option<&RegistryClient>,
    keep: &BTreeSet<String>,
) -> Result<SyncStats> {
    let plan = plan_layout(resolution)?;
    let store = PackageStore::from_env()?;

    // Phase 1: every unique package version present in the store.
    let mut store_dirs: HashMap<(String, String), std::path::PathBuf> = HashMap::new();
    let unique_ids: BTreeSet<&(String, String)> = plan
        .values()
        .map(|p| {
            resolution
                .packages
                .get_key_value(&(p.name.clone(), p.version.clone()))
                .map(|(k, _)| k)
                .expect("plan only references resolved packages")
        })
        .collect();

    let mut misses = Vec::new();
    for id in unique_ids {
        let pkg = &resolution.packages[id];
        let integrity = Integrity::from_dist(pkg.integrity.as_deref(), pkg.shasum.as_deref())
            .with_context(|| format!("{}@{}", pkg.name, pkg.version))?;
        match store.lookup(&integrity) {
            Some(dir) => {
                store_dirs.insert(id.clone(), dir);
            }
            None => misses.push((id.clone(), pkg.tarball.clone(), integrity)),
        }
    }

    let downloaded = misses.len();
    if !misses.is_empty() {
        let fallback_registry;
        let registry = match registry {
            Some(r) => r,
            None => {
                fallback_registry = RegistryClient::from_env()?;
                &fallback_registry
            }
        };
        let fetched: Vec<Result<((String, String), std::path::PathBuf)>> = block_on(async {
            futures::stream::iter(misses.into_iter().map(|(id, tarball, integrity)| {
                let store = store.clone();
                async move {
                    let bytes = registry.download_tarball(&tarball).await?;
                    // Hashing + gunzip + untar are CPU-bound: run them on
                    // the blocking pool, off the download threads.
                    let store_dir =
                        tokio::task::spawn_blocking(move || store.put_tarball(&integrity, &bytes))
                            .await
                            .expect("store task panicked")
                            .with_context(|| format!("storing {}@{}", id.0, id.1))?;
                    Ok((id, store_dir))
                }
            }))
            .buffer_unordered(DOWNLOAD_CONCURRENCY)
            .collect()
            .await
        });
        for item in fetched {
            let (id, store_dir) = item?;
            store_dirs.insert(id, store_dir);
        }
    }

    // Phase 2: materialize the plan. BTreeMap order guarantees parents are
    // laid down before their nested children.
    let mut linked = 0usize;
    for (chain, planned) in &plan {
        let dest = dir.join(chain_to_path(chain));
        if installed_version_matches(&dest, &planned.name, &planned.version) {
            continue;
        }
        if dest.exists() {
            std::fs::remove_dir_all(&dest)
                .with_context(|| format!("replacing {}", dest.display()))?;
        }
        let store_dir = &store_dirs[&(planned.name.clone(), planned.version.clone())];
        store
            .materialize(store_dir, &dest)
            .with_context(|| format!("linking {}@{}", planned.name, planned.version))?;
        linked += 1;
    }

    // Phase 3: prune everything the plan doesn't claim (minus `keep`).
    let mut pruned = 0usize;
    prune_extraneous(&dir.join("node_modules"), &[], &plan, keep, &mut pruned)?;

    Ok(SyncStats {
        installed: plan.len(),
        downloaded,
        linked,
        pruned,
    })
}

/// Does `dest` already hold `name@version`? (Trusts package.json, which is
/// enough because materialized trees are only ever produced whole.)
fn installed_version_matches(dest: &Path, name: &str, version: &str) -> bool {
    let Ok(raw) = std::fs::read_to_string(dest.join("package.json")) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    v.get("name").and_then(|x| x.as_str()) == Some(name)
        && v.get("version").and_then(|x| x.as_str()) == Some(version)
}

/// Remove package directories under `nm_dir` that the plan doesn't place.
/// Recurses through planned packages' nested `node_modules`. Non-directory
/// entries (e.g. stray files, npm's `file:` symlinks) are left alone, as are
/// top-level names in `keep` (unmanaged manifest deps).
fn prune_extraneous(
    nm_dir: &Path,
    prefix: &[String],
    plan: &LayoutPlan,
    keep: &BTreeSet<String>,
    pruned: &mut usize,
) -> Result<()> {
    if !nm_dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(nm_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        if dir_name.starts_with('@') {
            // Scope directory: check each @scope/name child.
            for sub in std::fs::read_dir(entry.path())? {
                let sub = sub?;
                if !sub.file_type()?.is_dir() {
                    continue;
                }
                let full = format!("{dir_name}/{}", sub.file_name().to_string_lossy());
                prune_one(&sub.path(), &full, prefix, plan, keep, pruned)?;
            }
            // Drop the scope dir once emptied.
            if std::fs::read_dir(entry.path())?.next().is_none() {
                std::fs::remove_dir(entry.path())?;
            }
        } else {
            prune_one(&entry.path(), &dir_name, prefix, plan, keep, pruned)?;
        }
    }
    Ok(())
}

fn prune_one(
    path: &Path,
    name: &str,
    prefix: &[String],
    plan: &LayoutPlan,
    keep: &BTreeSet<String>,
    pruned: &mut usize,
) -> Result<()> {
    if prefix.is_empty() && keep.contains(name) {
        return Ok(());
    }
    let mut chain = prefix.to_vec();
    chain.push(name.to_string());
    if plan.contains_key(&chain) {
        prune_extraneous(&path.join("node_modules"), &chain, plan, keep, pruned)
    } else {
        std::fs::remove_dir_all(path).with_context(|| format!("pruning {}", path.display()))?;
        *pruned += 1;
        Ok(())
    }
}

fn report(resolution: &Resolution, stats: &SyncStats, started: Instant) {
    let cached = stats.installed.saturating_sub(stats.downloaded);
    println!(
        "{} packages installed in {:?} ({} downloaded, {} from cache, {} linked, {} pruned)",
        stats.installed,
        started.elapsed(),
        stats.downloaded,
        cached,
        stats.linked,
        stats.pruned,
    );
    for warning in &resolution.warnings {
        eprintln!("warning: {warning}");
    }
}

fn load_lockfile_if_present(dir: &Path) -> Result<Option<Lockfile>> {
    let path = dir.join(LOCKFILE_NAME);
    if path.is_file() {
        Ok(Some(Lockfile::load(&path)?))
    } else {
        Ok(None)
    }
}

/// Versions pinned by the current lockfile, used to keep resolution stable
/// across unrelated `add`s.
fn preferred_versions(lockfile: Option<&Lockfile>) -> HashMap<String, BTreeSet<String>> {
    let mut preferred: HashMap<String, BTreeSet<String>> = HashMap::new();
    if let Some(lock) = lockfile {
        for (name, version) in lock.packages.keys() {
            preferred
                .entry(name.clone())
                .or_default()
                .insert(version.clone());
        }
    }
    preferred
}

/// Parse `name`, `name@range`, `@scope/name`, `@scope/name@range`.
fn parse_add_spec(spec: &str) -> Result<(String, Option<String>)> {
    let split_at = if let Some(rest) = spec.strip_prefix('@') {
        // Scoped: the version separator is an `@` after the first `/`.
        rest.find('/')
            .and_then(|slash| rest[slash..].find('@').map(|at| 1 + slash + at))
    } else {
        spec.find('@')
    };
    let (name, range) = match split_at {
        Some(idx) => (
            spec[..idx].to_string(),
            Some(spec[idx + 1..].trim().to_string()).filter(|r| !r.is_empty()),
        ),
        None => (spec.to_string(), None),
    };
    validate_package_name(&name)?;
    Ok((name, range))
}

/// The pkg commands are synchronous CLI entry points; network work runs on a
/// scoped runtime per call.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("building tokio runtime")
        .block_on(fut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_add_specs() {
        assert_eq!(parse_add_spec("zod").unwrap(), ("zod".into(), None));
        assert_eq!(
            parse_add_spec("zod@^3.22.0").unwrap(),
            ("zod".into(), Some("^3.22.0".into()))
        );
        assert_eq!(
            parse_add_spec("@scope/pkg").unwrap(),
            ("@scope/pkg".into(), None)
        );
        assert_eq!(
            parse_add_spec("@scope/pkg@2.x").unwrap(),
            ("@scope/pkg".into(), Some("2.x".into()))
        );
        assert_eq!(
            parse_add_spec("left-pad@latest").unwrap(),
            ("left-pad".into(), Some("latest".into()))
        );
        assert!(parse_add_spec("UPPER@1").is_err());
        assert!(parse_add_spec("@scope").is_err());
    }
}
