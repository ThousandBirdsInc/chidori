//! Hoisted `node_modules` layout planning.
//!
//! Given a resolved package set, decide where each copy lives on disk. The
//! layout must satisfy the Node resolution algorithm that
//! `runtime::typescript::resolver` implements: an import of `dep` from a
//! package installed at `node_modules/a/node_modules/b` probes, in order,
//! `node_modules/a/node_modules/b/node_modules/dep`,
//! `node_modules/a/node_modules/dep`*, `node_modules/dep`.
//! (*per Node, every non-`node_modules` ancestor directory gets a probe.)
//!
//! Strategy is npm-style greedy hoisting: root dependencies claim the top
//! level, every transitive dependency is hoisted to the top when its name is
//! free there, and version conflicts are nested under their dependent. The
//! walk is breadth-first over sorted keys, so the plan is deterministic for a
//! given resolution.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::path::PathBuf;

use anyhow::{bail, Result};

use super::resolve::Resolution;

/// Guard against pathological version-conflict cycles (A1→B1→A2→B2→A1…),
/// which are effectively nonexistent in the real registry but would otherwise
/// nest forever.
const MAX_NESTING: usize = 32;

/// A planned install location. `chain` is the package-name path from the
/// project root, e.g. `["a", "b"]` = `node_modules/a/node_modules/b`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedPackage {
    pub name: String,
    pub version: String,
}

/// Map from location chain to the package installed there. BTreeMap ordering
/// puts parents before their nested children (a chain sorts before its
/// extensions), which materialization relies on.
pub type LayoutPlan = BTreeMap<Vec<String>, PlannedPackage>;

/// Compute the install location of every package copy.
pub fn plan_layout(resolution: &Resolution) -> Result<LayoutPlan> {
    let mut plan: LayoutPlan = BTreeMap::new();
    let mut queue: VecDeque<(Vec<String>, (String, String))> = VecDeque::new();

    // Root direct dependencies own the top level.
    for (name, version) in &resolution.roots {
        plan.insert(
            vec![name.clone()],
            PlannedPackage {
                name: name.clone(),
                version: version.clone(),
            },
        );
        queue.push_back((vec![name.clone()], (name.clone(), version.clone())));
    }

    while let Some((chain, id)) = queue.pop_front() {
        let Some(pkg) = resolution.packages.get(&id) else {
            bail!(
                "layout plan references unresolved package {}@{}",
                id.0,
                id.1
            );
        };
        for (dep_name, dep_version) in &pkg.dependencies {
            // Probe ancestor levels nearest-first, mirroring Node's walk-up.
            let mut satisfied = false;
            let mut shadowed = false;
            for prefix_len in (0..=chain.len()).rev() {
                let mut candidate = chain[..prefix_len].to_vec();
                candidate.push(dep_name.clone());
                if let Some(existing) = plan.get(&candidate) {
                    if existing.version == *dep_version {
                        satisfied = true;
                    } else {
                        // Nearest visible copy is the wrong version; anything
                        // above it is shadowed. Must nest below.
                        shadowed = true;
                    }
                    break;
                }
            }
            if satisfied {
                continue;
            }
            let new_chain = if shadowed {
                // Nest directly under the dependent.
                let mut c = chain.clone();
                c.push(dep_name.clone());
                c
            } else {
                // Name unused anywhere on the probe path: hoist to top level.
                vec![dep_name.clone()]
            };
            if new_chain.len() > MAX_NESTING {
                bail!(
                    "dependency nesting exceeded {MAX_NESTING} levels at {} — conflicting version cycle?",
                    new_chain.join(" > ")
                );
            }
            // The probe loop always checks `new_chain` (either at prefix 0 for
            // a hoist or at the full chain for a nest) before choosing it, so
            // the slot is guaranteed free.
            let prev = plan.insert(
                new_chain.clone(),
                PlannedPackage {
                    name: dep_name.clone(),
                    version: dep_version.clone(),
                },
            );
            debug_assert!(prev.is_none(), "layout slot {new_chain:?} double-planned");
            queue.push_back((new_chain, (dep_name.clone(), dep_version.clone())));
        }
    }
    Ok(plan)
}

/// On-disk path (relative to the project root) for a location chain:
/// `["a","b"]` -> `node_modules/a/node_modules/b`. Scoped names (`@s/x`)
/// expand to two path segments naturally via `join`.
pub fn chain_to_path(chain: &[String]) -> PathBuf {
    let mut path = PathBuf::new();
    for name in chain {
        path.push("node_modules");
        path.push(name);
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pkg::resolve::ResolvedPackage;

    fn pkg(name: &str, version: &str, deps: &[(&str, &str)]) -> ResolvedPackage {
        ResolvedPackage {
            name: name.into(),
            version: version.into(),
            tarball: format!("https://example.test/{name}-{version}.tgz"),
            integrity: None,
            shasum: None,
            dependencies: deps
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn resolution(roots: &[(&str, &str)], packages: Vec<ResolvedPackage>) -> Resolution {
        Resolution {
            roots: roots
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            packages: packages.into_iter().map(|p| (p.id(), p)).collect(),
            warnings: vec![],
        }
    }

    #[test]
    fn hoists_shared_transitive_dep() {
        // a -> c@1, b -> c@1: c hoists to top level, one copy.
        let r = resolution(
            &[("a", "1.0.0"), ("b", "1.0.0")],
            vec![
                pkg("a", "1.0.0", &[("c", "1.0.0")]),
                pkg("b", "1.0.0", &[("c", "1.0.0")]),
                pkg("c", "1.0.0", &[]),
            ],
        );
        let plan = plan_layout(&r).unwrap();
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[&vec!["c".to_string()]].version, "1.0.0");
    }

    #[test]
    fn nests_conflicting_versions() {
        // root -> c@2 directly; a -> c@1 nests under a.
        let r = resolution(
            &[("a", "1.0.0"), ("c", "2.0.0")],
            vec![
                pkg("a", "1.0.0", &[("c", "1.0.0")]),
                pkg("c", "2.0.0", &[]),
                pkg("c", "1.0.0", &[]),
            ],
        );
        let plan = plan_layout(&r).unwrap();
        assert_eq!(plan[&vec!["c".to_string()]].version, "2.0.0");
        assert_eq!(
            plan[&vec!["a".to_string(), "c".to_string()]].version,
            "1.0.0"
        );
    }

    #[test]
    fn first_hoist_wins_later_conflict_nests() {
        // a -> c@1 (hoists c@1), b -> c@2 (nests under b).
        let r = resolution(
            &[("a", "1.0.0"), ("b", "1.0.0")],
            vec![
                pkg("a", "1.0.0", &[("c", "1.0.0")]),
                pkg("b", "1.0.0", &[("c", "2.0.0")]),
                pkg("c", "1.0.0", &[]),
                pkg("c", "2.0.0", &[]),
            ],
        );
        let plan = plan_layout(&r).unwrap();
        assert_eq!(plan[&vec!["c".to_string()]].version, "1.0.0");
        assert_eq!(
            plan[&vec!["b".to_string(), "c".to_string()]].version,
            "2.0.0"
        );
    }

    #[test]
    fn circular_dependencies_terminate() {
        let r = resolution(
            &[("a", "1.0.0")],
            vec![
                pkg("a", "1.0.0", &[("b", "1.0.0")]),
                pkg("b", "1.0.0", &[("a", "1.0.0")]),
            ],
        );
        let plan = plan_layout(&r).unwrap();
        assert_eq!(plan.len(), 2);
    }

    #[test]
    fn nested_conflict_dep_resolves_against_nested_copy() {
        // root -> c@2; a -> c@1; c@1 -> d@1; d hoists to top.
        let r = resolution(
            &[("a", "1.0.0"), ("c", "2.0.0")],
            vec![
                pkg("a", "1.0.0", &[("c", "1.0.0")]),
                pkg("c", "2.0.0", &[]),
                pkg("c", "1.0.0", &[("d", "1.0.0")]),
                pkg("d", "1.0.0", &[]),
            ],
        );
        let plan = plan_layout(&r).unwrap();
        assert_eq!(plan[&vec!["d".to_string()]].version, "1.0.0");
        assert!(plan.contains_key(&vec!["a".to_string(), "c".to_string()]));
    }

    #[test]
    fn chain_paths() {
        assert_eq!(
            chain_to_path(&["a".into(), "b".into()]),
            PathBuf::from("node_modules/a/node_modules/b")
        );
        assert_eq!(
            chain_to_path(&["@s/x".into()]),
            PathBuf::from("node_modules/@s/x")
        );
    }
}
