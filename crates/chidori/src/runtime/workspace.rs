//! Workspace file store scaffolding: manifest, file/deleted entries, and the
//! path-sanitized read/write/list/delete surface. Not yet wired into the
//! runtime — hence the module-wide `dead_code` allow, same as the other
//! staged runtime modules (`native.rs`, `snapshot.rs`, `capability.rs`,
//! `vfs.rs`). Remove the allow when the first call site lands.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

const WORKSPACE_MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceManifest {
    version: u32,
    #[serde(default)]
    manifest_version: u64,
    #[serde(default)]
    active_attempt: u64,
    #[serde(default)]
    files: BTreeMap<String, WorkspaceFileEntry>,
    #[serde(default)]
    deleted: BTreeMap<String, WorkspaceDeletedEntry>,
}

impl Default for WorkspaceManifest {
    fn default() -> Self {
        Self {
            version: WORKSPACE_MANIFEST_VERSION,
            manifest_version: 0,
            active_attempt: workspace_attempt().unwrap_or(0),
            files: BTreeMap::new(),
            deleted: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceFileEntry {
    status: WorkspaceFileStatus,
    sha256: String,
    bytes: u64,
    language: Option<String>,
    attempt: Option<u64>,
    updated_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum WorkspaceFileStatus {
    Complete,
    Writing,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceDeletedEntry {
    attempt: Option<u64>,
    reason: Option<String>,
}

pub fn list(root: &Path, complete_only: bool) -> Result<Value, String> {
    let manifest = read_manifest(root)?;
    let entries = manifest
        .files
        .into_iter()
        .filter(|(_, entry)| !complete_only || entry.status == WorkspaceFileStatus::Complete)
        .map(|(path, entry)| {
            serde_json::json!({
                "path": path,
                "status": entry.status,
                "sha256": entry.sha256,
                "bytes": entry.bytes,
                "language": entry.language,
                "attempt": entry.attempt,
                "updatedAt": entry.updated_at,
            })
        })
        .collect::<Vec<_>>();
    Ok(Value::Array(entries))
}

pub fn read(root: &Path, path: &str) -> Result<String, String> {
    let relative = sanitize_path(path)?;
    let absolute = workspace_path(root, &relative)?;
    ensure_no_symlink_path(root, &absolute)?;
    std::fs::read_to_string(&absolute)
        .map_err(|err| format!("workspace.read {}: {err}", relative.display()))
}

pub fn write(root: &Path, path: &str, content: &str, options: &Value) -> Result<Value, String> {
    let relative = sanitize_path(path)?;
    let absolute = workspace_path(root, &relative)?;
    ensure_no_symlink_path(root, &absolute)?;
    ensure_layout(root)?;
    if let Some(parent) = absolute.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("create workspace parent {}: {err}", parent.display()))?;
    }

    let tmp = root
        .join(".generation")
        .join("tmp")
        .join(format!("write-{}", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, content.as_bytes())
        .map_err(|err| format!("workspace temp write {}: {err}", tmp.display()))?;
    std::fs::rename(&tmp, &absolute).map_err(|err| {
        let _ = std::fs::remove_file(&tmp);
        format!("workspace atomic rename {}: {err}", absolute.display())
    })?;

    let mut manifest = read_manifest(root)?;
    let path = relative_path_string(&relative)?;
    let entry = WorkspaceFileEntry {
        status: WorkspaceFileStatus::Complete,
        sha256: sha256_hex(content.as_bytes()),
        bytes: content.len() as u64,
        language: options
            .get("language")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| Some(language_for_path(&relative))),
        attempt: workspace_attempt(),
        updated_at: Some(chrono::Utc::now().to_rfc3339()),
    };
    if let Some(attempt) = workspace_attempt() {
        manifest.active_attempt = attempt;
    }
    manifest.files.insert(path.clone(), entry.clone());
    manifest.deleted.remove(&path);
    write_manifest(&manifest_path(root), &manifest)?;

    Ok(serde_json::json!({
        "path": path,
        "status": entry.status,
        "sha256": entry.sha256,
        "bytes": entry.bytes,
        "language": entry.language,
        "attempt": entry.attempt,
        "updatedAt": entry.updated_at,
    }))
}

pub fn delete(root: &Path, path: &str, reason: Option<&str>) -> Result<Value, String> {
    let relative = sanitize_path(path)?;
    let absolute = workspace_path(root, &relative)?;
    ensure_no_symlink_path(root, &absolute)?;
    if absolute.exists() {
        std::fs::remove_file(&absolute)
            .map_err(|err| format!("workspace.delete {}: {err}", relative.display()))?;
    }
    let mut manifest = read_manifest(root)?;
    let path = relative_path_string(&relative)?;
    manifest.files.remove(&path);
    manifest.deleted.insert(
        path,
        WorkspaceDeletedEntry {
            attempt: workspace_attempt(),
            reason: reason.map(ToOwned::to_owned),
        },
    );
    write_manifest(&manifest_path(root), &manifest)?;
    Ok(Value::Null)
}

pub fn manifest(root: &Path) -> Result<Value, String> {
    read_manifest(root)
        .and_then(|manifest| serde_json::to_value(manifest).map_err(|e| e.to_string()))
}

fn read_manifest(root: &Path) -> Result<WorkspaceManifest, String> {
    ensure_layout(root)?;
    let path = manifest_path(root);
    if !path.exists() {
        let manifest = WorkspaceManifest::default();
        write_manifest(&path, &manifest)?;
        return Ok(manifest);
    }
    let bytes =
        std::fs::read(&path).map_err(|err| format!("read manifest {}: {err}", path.display()))?;
    let manifest: WorkspaceManifest = serde_json::from_slice(&bytes)
        .map_err(|err| format!("parse manifest {}: {err}", path.display()))?;
    if manifest.version != WORKSPACE_MANIFEST_VERSION {
        return Err(format!(
            "unsupported workspace manifest version {}",
            manifest.version
        ));
    }
    Ok(manifest)
}

fn write_manifest(path: &Path, manifest: &WorkspaceManifest) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("create manifest dir {}: {err}", parent.display()))?;
    }
    let bytes =
        serde_json::to_vec_pretty(manifest).map_err(|err| format!("serialize manifest: {err}"))?;
    std::fs::write(path, bytes).map_err(|err| format!("write manifest {}: {err}", path.display()))
}

fn ensure_layout(root: &Path) -> Result<(), String> {
    std::fs::create_dir_all(root.join(".generation").join("tmp"))
        .map_err(|err| format!("create workspace metadata dirs {}: {err}", root.display()))
}

fn manifest_path(root: &Path) -> PathBuf {
    root.join(".generation").join("manifest.json")
}

fn sanitize_path(path: &str) -> Result<PathBuf, String> {
    if path.trim().is_empty() {
        return Err("workspace path must not be empty".to_string());
    }
    let mut relative = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("invalid workspace path: {path}"));
            }
        }
    }
    if relative.as_os_str().is_empty() {
        Err(format!("invalid workspace path: {path}"))
    } else {
        Ok(relative)
    }
}

fn workspace_path(root: &Path, relative: &Path) -> Result<PathBuf, String> {
    let absolute = root.join(relative);
    if absolute.starts_with(root) {
        Ok(absolute)
    } else {
        Err(format!(
            "workspace path escapes root: {}",
            relative.display()
        ))
    }
}

fn ensure_no_symlink_path(root: &Path, absolute: &Path) -> Result<(), String> {
    let mut current = root.to_path_buf();
    let relative = absolute
        .strip_prefix(root)
        .map_err(|_| format!("workspace path escapes root: {}", absolute.display()))?;
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(format!("invalid workspace path: {}", relative.display()));
        };
        current.push(part);
        if let Ok(metadata) = std::fs::symlink_metadata(&current) {
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "workspace path must not traverse a symlink: {}",
                    current.display()
                ));
            }
        }
    }
    Ok(())
}

fn relative_path_string(path: &Path) -> Result<String, String> {
    let value = path
        .components()
        .map(|component| match component {
            Component::Normal(part) => Ok(part.to_string_lossy().to_string()),
            _ => Err(format!("invalid workspace path: {}", path.display())),
        })
        .collect::<Result<Vec<_>, _>>()?
        .join("/");
    if value.is_empty() {
        Err(format!("invalid workspace path: {}", path.display()))
    } else {
        Ok(value)
    }
}

fn workspace_attempt() -> Option<u64> {
    std::env::var("CHIDORI_WORKSPACE_ATTEMPT")
        .ok()
        .and_then(|value| value.parse().ok())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn language_for_path(path: &Path) -> String {
    match path.extension().and_then(|ext| ext.to_str()).unwrap_or("") {
        "ts" | "tsx" => "typescript",
        "js" | "jsx" => "javascript",
        "json" => "json",
        "md" | "mdx" => "markdown",
        "py" => "python",
        "rs" => "rust",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "css" => "css",
        "html" => "html",
        _ => "text",
    }
    .to_string()
}
