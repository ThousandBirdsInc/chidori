//! TypeScript module-graph collection for snapshot manifests.
//!
//! The durable run manifest records the fingerprints and import graph of every
//! module an agent pulls in, so a resume can validate that the on-disk source
//! still matches what was recorded. This is engine-agnostic: it walks relative
//! and `node:` imports, transpile-free, purely to describe the graph. The
//! pure-Rust engine links and runs the actual module graph itself (see
//! `rust_engine::load_module_source`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::runtime::snapshot::{
    RuntimePolicy, SnapshotModuleGraphEntry, SnapshotModuleImport, SourceFingerprint,
};
use crate::runtime::typescript::transpile::validate_imports;

/// Module fingerprints for every dependency of `path` (excluding the entry
/// itself), used to detect source drift on resume.
pub fn snapshot_module_fingerprints(
    path: &Path,
    source: &str,
    policy: &RuntimePolicy,
) -> Result<Vec<SourceFingerprint>> {
    Ok(snapshot_modules(path, source, policy)?.0)
}

/// The full module import graph (entry + dependencies) for the manifest.
pub fn snapshot_module_graph(
    path: &Path,
    source: &str,
    policy: &RuntimePolicy,
) -> Result<Vec<SnapshotModuleGraphEntry>> {
    Ok(snapshot_modules(path, source, policy)?.1)
}

/// Both manifest views of the module walk — dependency fingerprints and the
/// full import graph — from a single collection pass, so callers that need
/// both (the per-run scaffold persister) read each module file once instead
/// of twice.
pub fn snapshot_modules(
    path: &Path,
    source: &str,
    policy: &RuntimePolicy,
) -> Result<(Vec<SourceFingerprint>, Vec<SnapshotModuleGraphEntry>)> {
    let entry_path = stable_path(path);
    let mut builder = SnapshotModuleBuilder::new(policy);
    builder.collect(path, source)?;
    let fingerprints = builder
        .modules
        .iter()
        .filter(|module| module.path != entry_path)
        .map(|module| SourceFingerprint::from_source(module.path.clone(), &module.source))
        .collect();
    let graph = builder
        .modules
        .iter()
        .map(|module| SnapshotModuleGraphEntry {
            path: module.path.clone(),
            imports: module
                .imports
                .iter()
                .map(|import| SnapshotModuleImport {
                    specifier: import.specifier.clone(),
                    resolved_path: import.resolved_path.clone(),
                })
                .collect(),
        })
        .collect();
    Ok((fingerprints, graph))
}

struct SnapshotModuleBuilder<'policy> {
    policy: &'policy RuntimePolicy,
    modules: Vec<SnapshotModule>,
    module_keys: HashMap<PathBuf, String>,
    seen: HashSet<PathBuf>,
    visiting: HashSet<PathBuf>,
}

struct SnapshotModule {
    path: PathBuf,
    #[allow(dead_code)]
    key: String,
    source: String,
    imports: Vec<ResolvedSnapshotImport>,
}

#[derive(Clone)]
struct ResolvedSnapshotImport {
    specifier: String,
    resolved_path: Option<PathBuf>,
}

impl<'policy> SnapshotModuleBuilder<'policy> {
    fn new(policy: &'policy RuntimePolicy) -> Self {
        Self {
            policy,
            modules: Vec::new(),
            module_keys: HashMap::new(),
            seen: HashSet::new(),
            visiting: HashSet::new(),
        }
    }

    fn collect(&mut self, path: &Path, source: &str) -> Result<()> {
        let path = stable_path(path);
        if self.seen.contains(&path) {
            return Ok(());
        }
        if !self.visiting.insert(path.clone()) {
            anyhow::bail!(
                "{}: cyclic TypeScript imports are not supported by the snapshot scaffold",
                path.display()
            );
        }

        let imports = resolved_snapshot_imports(&path, source, self.policy)?;
        for module_path in imports
            .iter()
            .filter_map(|import| import.resolved_path.as_ref())
        {
            // node:* builtins resolve to synthetic paths under
            // `__node_builtins__/`; their bodies come from the shim registry,
            // not the filesystem.
            let module_source =
                if let Some(shim) = crate::runtime::typescript::builtins::source_for(module_path) {
                    shim.to_string()
                } else {
                    std::fs::read_to_string(module_path)
                        .with_context(|| format!("Failed to read {}", module_path.display()))?
                };
            self.collect(module_path, &module_source)?;
        }

        self.visiting.remove(&path);
        self.seen.insert(path.clone());
        let key = snapshot_module_key(&path);
        self.module_keys.insert(path.clone(), key.clone());
        self.modules.push(SnapshotModule {
            path,
            key,
            source: source.to_string(),
            imports,
        });
        Ok(())
    }
}

fn resolved_snapshot_imports(
    path: &Path,
    source: &str,
    policy: &RuntimePolicy,
) -> Result<Vec<ResolvedSnapshotImport>> {
    validate_imports(path, source, policy.typescript_imports).map(|imports| {
        imports
            .into_iter()
            .map(|import| ResolvedSnapshotImport {
                specifier: import.specifier,
                resolved_path: import.resolved_path.map(|path| stable_path(&path)),
            })
            .collect()
    })
}

fn stable_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn snapshot_module_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
