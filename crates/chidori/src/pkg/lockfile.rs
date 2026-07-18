//! `chidori.lock.jsonl` — a strictly sorted JSON-lines lockfile.
//!
//! Line 1 is a header carrying the lockfile version, the ranges that were
//! requested (so `chidori install` can tell whether the lockfile is in sync
//! with `package.json`), and the root's resolved direct dependencies. Every
//! following line is one locked package, sorted by name then ascending
//! semver.
//!
//! The JSONL + strict sort combination is deliberately git-friendly: two
//! branches adding different dependencies touch disjoint lines, so merges
//! apply cleanly instead of conflicting over one giant JSON blob.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use nodejs_semver::Version;
use serde::{Deserialize, Serialize};

use super::resolve::{Resolution, ResolvedPackage};

pub const LOCKFILE_NAME: &str = "chidori.lock.jsonl";
const LOCKFILE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Header {
    /// Format marker; bump on breaking layout changes.
    #[serde(rename = "chidoriLockfile")]
    version: u32,
    /// Ranges requested by package.json at lock time: name -> range.
    /// Prod and dev are tracked separately so sync checks are exact.
    #[serde(default)]
    requested: BTreeMap<String, String>,
    #[serde(default, rename = "requestedDev")]
    requested_dev: BTreeMap<String, String>,
    /// Root direct dependencies as resolved: name -> exact version.
    #[serde(default)]
    roots: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockedPackage {
    pub name: String,
    pub version: String,
    pub resolved: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shasum: Option<String>,
    /// Resolved edges: dep name -> exact version.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct Lockfile {
    pub requested: BTreeMap<String, String>,
    pub requested_dev: BTreeMap<String, String>,
    pub roots: BTreeMap<String, String>,
    /// Keyed by (name, version).
    pub packages: BTreeMap<(String, String), LockedPackage>,
}

impl Lockfile {
    pub fn from_resolution(
        resolution: &Resolution,
        requested: BTreeMap<String, String>,
        requested_dev: BTreeMap<String, String>,
    ) -> Self {
        let packages = resolution
            .packages
            .values()
            .map(|p| (p.id(), locked_from(p)))
            .collect();
        Self {
            requested,
            requested_dev,
            roots: resolution.roots.clone(),
            packages,
        }
    }

    /// Rebuild a `Resolution` (for layout planning) without touching the
    /// network.
    pub fn to_resolution(&self) -> Resolution {
        Resolution {
            roots: self.roots.clone(),
            packages: self
                .packages
                .iter()
                .map(|(id, p)| {
                    (
                        id.clone(),
                        ResolvedPackage {
                            name: p.name.clone(),
                            version: p.version.clone(),
                            tarball: p.resolved.clone(),
                            integrity: p.integrity.clone(),
                            shasum: p.shasum.clone(),
                            dependencies: p.dependencies.clone(),
                        },
                    )
                })
                .collect(),
            warnings: vec![],
        }
    }

    /// True when this lockfile was produced from exactly these requested
    /// ranges — i.e. `install` can trust it without re-resolving.
    pub fn matches_manifest(
        &self,
        deps: &BTreeMap<String, String>,
        dev_deps: &BTreeMap<String, String>,
    ) -> bool {
        self.requested == *deps && self.requested_dev == *dev_deps
    }

    pub fn load(path: &Path) -> Result<Self> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut lines = raw.lines().filter(|l| !l.trim().is_empty());
        let header_line = match lines.next() {
            Some(l) => l,
            None => bail!("{} is empty", path.display()),
        };
        let header: Header = serde_json::from_str(header_line)
            .with_context(|| format!("parsing {} header", path.display()))?;
        if header.version != LOCKFILE_VERSION {
            bail!(
                "{} has lockfile version {}, this chidori supports {}",
                path.display(),
                header.version,
                LOCKFILE_VERSION
            );
        }
        let mut packages = BTreeMap::new();
        for line in lines {
            let pkg: LockedPackage = serde_json::from_str(line)
                .with_context(|| format!("parsing lockfile entry: {line}"))?;
            packages.insert((pkg.name.clone(), pkg.version.clone()), pkg);
        }
        Ok(Self {
            requested: header.requested,
            requested_dev: header.requested_dev,
            roots: header.roots,
            packages,
        })
    }

    /// Serialize: header line, then packages sorted by name and semver.
    pub fn to_jsonl(&self) -> String {
        let header = Header {
            version: LOCKFILE_VERSION,
            requested: self.requested.clone(),
            requested_dev: self.requested_dev.clone(),
            roots: self.roots.clone(),
        };
        let mut out = serde_json::to_string(&header).expect("header serializes");
        out.push('\n');

        let mut entries: Vec<&LockedPackage> = self.packages.values().collect();
        entries.sort_by(|a, b| {
            a.name.cmp(&b.name).then_with(|| {
                match (a.version.parse::<Version>(), b.version.parse::<Version>()) {
                    (Ok(av), Ok(bv)) => av.cmp(&bv),
                    _ => a.version.cmp(&b.version),
                }
            })
        });
        for pkg in entries {
            out.push_str(&serde_json::to_string(pkg).expect("package serializes"));
            out.push('\n');
        }
        out
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        std::fs::write(path, self.to_jsonl()).with_context(|| format!("writing {}", path.display()))
    }
}

fn locked_from(p: &ResolvedPackage) -> LockedPackage {
    LockedPackage {
        name: p.name.clone(),
        version: p.version.clone(),
        resolved: p.tarball.clone(),
        integrity: p.integrity.clone(),
        shasum: p.shasum.clone(),
        dependencies: p.dependencies.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Lockfile {
        let mut lf = Lockfile::default();
        lf.requested.insert("b".into(), "^2.0.0".into());
        lf.requested.insert("a".into(), "^1.0.0".into());
        lf.roots.insert("a".into(), "1.2.3".into());
        lf.roots.insert("b".into(), "2.0.1".into());
        for (name, version) in [("b", "2.0.1"), ("a", "1.2.3"), ("a", "1.10.0")] {
            lf.packages.insert(
                (name.into(), version.into()),
                LockedPackage {
                    name: name.into(),
                    version: version.into(),
                    resolved: format!("https://r.test/{name}/-/{name}-{version}.tgz"),
                    integrity: Some("sha512-abc".into()),
                    shasum: None,
                    dependencies: BTreeMap::new(),
                },
            );
        }
        lf
    }

    #[test]
    fn roundtrips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOCKFILE_NAME);
        let lf = sample();
        lf.save(&path).unwrap();
        let loaded = Lockfile::load(&path).unwrap();
        assert_eq!(loaded.requested, lf.requested);
        assert_eq!(loaded.roots, lf.roots);
        assert_eq!(loaded.packages, lf.packages);
    }

    #[test]
    fn output_is_strictly_sorted_and_line_oriented() {
        let text = sample().to_jsonl();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains("chidoriLockfile"));
        // a@1.2.3 sorts before a@1.10.0 (semver, not lexicographic), then b.
        assert!(lines[1].contains(r#""version":"1.2.3""#));
        assert!(lines[2].contains(r#""version":"1.10.0""#));
        assert!(lines[3].contains(r#""name":"b""#));
        // Deterministic: serializing twice is byte-identical.
        assert_eq!(text, sample().to_jsonl());
    }

    #[test]
    fn matches_manifest_detects_drift() {
        let lf = sample();
        assert!(lf.matches_manifest(&lf.requested.clone(), &BTreeMap::new()));
        let mut changed = lf.requested.clone();
        changed.insert("c".into(), "*".into());
        assert!(!lf.matches_manifest(&changed, &BTreeMap::new()));
    }
}
