#![allow(dead_code)]
//! In-memory, snapshot-resident virtual filesystem backing `node:fs`.
//!
//! See `docs/captured-effects-vfs-crypto-timers.md`. The VFS is plain data that
//! rides the snapshot manifest, so reads/writes are deterministic and survive a
//! suspend → restore: a write never touches the host disk, and a restore
//! reconstructs the identical tree. The only host-disk touch is an explicit
//! pre-run seed (see `RuntimeContext` / `CHIDORI_VFS_SEED`).
//!
//! Paths are normalized to absolute, `/`-rooted, slash-separated strings with
//! `.`/`..` resolved and `..` past root clamped (it never escapes to host
//! disk). Iteration is `BTreeMap`-ordered so `readdir` is stable across runs,
//! and file times are *logical* (derived from an effect sequence number, not
//! wall-clock) so `stat` output is deterministic.

use std::collections::BTreeMap;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// A node in the virtual filesystem: a file with bytes, or a directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VfsNode {
    File {
        /// File contents, base64-encoded so binary survives JSON serialization.
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
        /// Logical modification time: the effect sequence at last write.
        mtime_seq: u64,
    },
    Dir,
}

/// The virtual filesystem tree, keyed by normalized absolute path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vfs {
    nodes: BTreeMap<String, VfsNode>,
}

impl Default for Vfs {
    fn default() -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert("/".to_string(), VfsNode::Dir);
        Self { nodes }
    }
}

impl Vfs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read a file's bytes. Errors if absent or a directory.
    pub fn read(&self, path: &str) -> Result<Vec<u8>, String> {
        let path = normalize(path);
        match self.nodes.get(&path) {
            Some(VfsNode::File { bytes, .. }) => Ok(bytes.clone()),
            Some(VfsNode::Dir) => Err(format!("EISDIR: illegal operation on a directory, {path}")),
            None => Err(format!("ENOENT: no such file or directory, open '{path}'")),
        }
    }

    /// Read a file as UTF-8 text.
    pub fn read_text(&self, path: &str) -> Result<String, String> {
        let bytes = self.read(path)?;
        String::from_utf8(bytes).map_err(|_| format!("EILSEQ: file is not valid UTF-8, {path}"))
    }

    /// Write (creating or truncating) a file. Parent directories must exist
    /// unless they are created implicitly here — Node requires the parent to
    /// exist, so we mirror that and error if it doesn't.
    pub fn write(&mut self, path: &str, bytes: Vec<u8>, mtime_seq: u64) -> Result<(), String> {
        let path = normalize(path);
        if path == "/" {
            return Err("EISDIR: illegal operation on a directory, '/'".to_string());
        }
        if let Some(VfsNode::Dir) = self.nodes.get(&path) {
            return Err(format!("EISDIR: illegal operation on a directory, {path}"));
        }
        let parent = parent_of(&path);
        match self.nodes.get(&parent) {
            Some(VfsNode::Dir) => {}
            Some(VfsNode::File { .. }) => {
                return Err(format!("ENOTDIR: not a directory, {parent}"));
            }
            None => {
                return Err(format!("ENOENT: no such file or directory, open '{path}'"));
            }
        }
        self.nodes.insert(path, VfsNode::File { bytes, mtime_seq });
        Ok(())
    }

    /// Append bytes to a file, creating it if absent.
    pub fn append(&mut self, path: &str, extra: &[u8], mtime_seq: u64) -> Result<(), String> {
        let mut bytes = match self.read(path) {
            Ok(existing) => existing,
            Err(e) if e.starts_with("ENOENT") => Vec::new(),
            Err(e) => return Err(e),
        };
        bytes.extend_from_slice(extra);
        self.write(path, bytes, mtime_seq)
    }

    /// Create a directory. With `recursive`, creates missing parents and
    /// succeeds if the directory already exists (matching `fs.mkdir`'s
    /// `recursive: true`).
    pub fn mkdir(&mut self, path: &str, recursive: bool) -> Result<(), String> {
        let path = normalize(path);
        if path == "/" {
            if recursive {
                return Ok(());
            }
            return Err("EEXIST: file already exists, mkdir '/'".to_string());
        }
        match self.nodes.get(&path) {
            Some(VfsNode::Dir) if recursive => return Ok(()),
            Some(_) => return Err(format!("EEXIST: file already exists, mkdir '{path}'")),
            None => {}
        }
        let parent = parent_of(&path);
        match self.nodes.get(&parent) {
            Some(VfsNode::Dir) => {}
            Some(VfsNode::File { .. }) => {
                return Err(format!("ENOTDIR: not a directory, {parent}"))
            }
            None => {
                if recursive {
                    self.mkdir(&parent, true)?;
                } else {
                    return Err(format!("ENOENT: no such file or directory, mkdir '{path}'"));
                }
            }
        }
        self.nodes.insert(path, VfsNode::Dir);
        Ok(())
    }

    /// List the immediate children (base names) of a directory, sorted.
    pub fn readdir(&self, path: &str) -> Result<Vec<String>, String> {
        let path = normalize(path);
        match self.nodes.get(&path) {
            Some(VfsNode::Dir) => {}
            Some(VfsNode::File { .. }) => {
                return Err(format!("ENOTDIR: not a directory, scandir '{path}'"))
            }
            None => {
                return Err(format!(
                    "ENOENT: no such file or directory, scandir '{path}'"
                ))
            }
        }
        let prefix = if path == "/" {
            "/".to_string()
        } else {
            format!("{path}/")
        };
        let mut out = Vec::new();
        for key in self.nodes.keys() {
            if key == &path {
                continue;
            }
            if let Some(rest) = key.strip_prefix(&prefix) {
                if !rest.is_empty() && !rest.contains('/') {
                    out.push(rest.to_string());
                }
            }
        }
        // BTreeMap keys are already sorted, so `out` is sorted.
        Ok(out)
    }

    /// Remove a file or (with `recursive`) a directory tree. With `force`,
    /// a missing path is not an error (matching `fs.rm`'s `force: true`).
    pub fn remove(&mut self, path: &str, recursive: bool, force: bool) -> Result<(), String> {
        let path = normalize(path);
        if path == "/" {
            return Err("EBUSY: cannot remove the filesystem root".to_string());
        }
        match self.nodes.get(&path) {
            None => {
                if force {
                    return Ok(());
                }
                return Err(format!(
                    "ENOENT: no such file or directory, unlink '{path}'"
                ));
            }
            Some(VfsNode::File { .. }) => {
                self.nodes.remove(&path);
                Ok(())
            }
            Some(VfsNode::Dir) => {
                let children = self.readdir(&path)?;
                if !children.is_empty() && !recursive {
                    return Err(format!("ENOTEMPTY: directory not empty, rmdir '{path}'"));
                }
                let prefix = format!("{path}/");
                let to_remove: Vec<String> = self
                    .nodes
                    .keys()
                    .filter(|k| *k == &path || k.starts_with(&prefix))
                    .cloned()
                    .collect();
                for key in to_remove {
                    self.nodes.remove(&key);
                }
                Ok(())
            }
        }
    }

    /// Rename/move a file or directory subtree.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<(), String> {
        let from = normalize(from);
        let to = normalize(to);
        if from == "/" || to == "/" {
            return Err("EBUSY: cannot rename the filesystem root".to_string());
        }
        if !self.nodes.contains_key(&from) {
            return Err(format!(
                "ENOENT: no such file or directory, rename '{from}'"
            ));
        }
        let to_parent = parent_of(&to);
        if !matches!(self.nodes.get(&to_parent), Some(VfsNode::Dir)) {
            return Err(format!("ENOENT: no such file or directory, rename '{to}'"));
        }
        // Collect the moving subtree (the node itself plus any descendants).
        let from_prefix = format!("{from}/");
        let moving: Vec<(String, VfsNode)> = self
            .nodes
            .iter()
            .filter(|(k, _)| *k == &from || k.starts_with(&from_prefix))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (key, _) in &moving {
            self.nodes.remove(key);
        }
        for (key, node) in moving {
            let new_key = if key == from {
                to.clone()
            } else {
                format!("{to}{}", &key[from.len()..])
            };
            self.nodes.insert(new_key, node);
        }
        Ok(())
    }

    pub fn exists(&self, path: &str) -> bool {
        self.nodes.contains_key(&normalize(path))
    }

    /// `stat`-style metadata as a JSON object with deterministic, logical times.
    pub fn stat(&self, path: &str) -> Result<Value, String> {
        let path = normalize(path);
        match self.nodes.get(&path) {
            Some(VfsNode::File { bytes, mtime_seq }) => Ok(json!({
                "isFile": true,
                "isDirectory": false,
                "size": bytes.len(),
                "mtimeSeq": mtime_seq,
            })),
            Some(VfsNode::Dir) => Ok(json!({
                "isFile": false,
                "isDirectory": true,
                "size": 0,
                "mtimeSeq": 0,
            })),
            None => Err(format!("ENOENT: no such file or directory, stat '{path}'")),
        }
    }

    /// Insert a file directly (used by the host seed path). Creates parent
    /// directories recursively.
    pub fn seed_file(&mut self, path: &str, bytes: Vec<u8>) {
        let path = normalize(path);
        let parent = parent_of(&path);
        let _ = self.mkdir(&parent, true);
        self.nodes.insert(
            path,
            VfsNode::File {
                bytes,
                mtime_seq: 0,
            },
        );
    }
}

/// Normalize a path to an absolute, `/`-rooted, slash-separated form with
/// `.`/`..` resolved. Backslashes are treated as separators for friendliness.
/// `..` past the root clamps at the root — it never reaches host disk.
pub fn normalize(path: &str) -> String {
    let unified = path.replace('\\', "/");
    let mut stack: Vec<&str> = Vec::new();
    for segment in unified.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    if stack.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", stack.join("/"))
    }
}

/// The normalized parent directory of a normalized path.
fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_string(),
        Some(idx) => path[..idx].to_string(),
        None => "/".to_string(),
    }
}

/// Base64 (de)serialization for file bytes, keeping binary content JSON-safe.
mod base64_bytes {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        serializer.serialize_str(&encoded)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(s.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_resolves_and_clamps() {
        assert_eq!(normalize("/a/b/../c"), "/a/c");
        assert_eq!(normalize("a/./b"), "/a/b");
        assert_eq!(normalize("/../../etc/passwd"), "/etc/passwd"); // clamped, not escaped
        assert_eq!(normalize(""), "/");
        assert_eq!(normalize("/"), "/");
        assert_eq!(normalize("a\\b"), "/a/b");
    }

    #[test]
    fn write_then_read_round_trips() {
        let mut vfs = Vfs::new();
        vfs.write("/hello.txt", b"hi".to_vec(), 1).unwrap();
        assert_eq!(vfs.read_text("/hello.txt").unwrap(), "hi");
    }

    #[test]
    fn write_requires_existing_parent() {
        let mut vfs = Vfs::new();
        let err = vfs.write("/nope/file.txt", b"x".to_vec(), 1).unwrap_err();
        assert!(err.starts_with("ENOENT"));
        vfs.mkdir("/nope", false).unwrap();
        vfs.write("/nope/file.txt", b"x".to_vec(), 2).unwrap();
    }

    #[test]
    fn mkdir_recursive_creates_parents() {
        let mut vfs = Vfs::new();
        vfs.mkdir("/a/b/c", true).unwrap();
        assert!(vfs.exists("/a"));
        assert!(vfs.exists("/a/b"));
        assert!(vfs.exists("/a/b/c"));
        // recursive mkdir on an existing dir is a no-op
        vfs.mkdir("/a/b", true).unwrap();
        // non-recursive on existing dir errors
        assert!(vfs.mkdir("/a/b", false).is_err());
    }

    #[test]
    fn readdir_lists_immediate_children_sorted() {
        let mut vfs = Vfs::new();
        vfs.mkdir("/d", false).unwrap();
        vfs.write("/d/z.txt", b"".to_vec(), 1).unwrap();
        vfs.write("/d/a.txt", b"".to_vec(), 1).unwrap();
        vfs.mkdir("/d/sub", false).unwrap();
        vfs.write("/d/sub/deep.txt", b"".to_vec(), 1).unwrap();
        assert_eq!(vfs.readdir("/d").unwrap(), vec!["a.txt", "sub", "z.txt"]);
    }

    #[test]
    fn remove_file_and_recursive_dir() {
        let mut vfs = Vfs::new();
        vfs.mkdir("/d", false).unwrap();
        vfs.write("/d/f.txt", b"x".to_vec(), 1).unwrap();
        assert!(vfs.remove("/d", false, false).is_err()); // not empty
        vfs.remove("/d", true, false).unwrap();
        assert!(!vfs.exists("/d"));
        assert!(!vfs.exists("/d/f.txt"));
        // force makes a missing path a no-op
        vfs.remove("/gone", false, true).unwrap();
    }

    #[test]
    fn rename_moves_subtree() {
        let mut vfs = Vfs::new();
        vfs.mkdir("/a", false).unwrap();
        vfs.write("/a/f.txt", b"hi".to_vec(), 1).unwrap();
        vfs.rename("/a", "/b").unwrap();
        assert!(!vfs.exists("/a"));
        assert_eq!(vfs.read_text("/b/f.txt").unwrap(), "hi");
    }

    #[test]
    fn serde_round_trip_preserves_binary() {
        let mut vfs = Vfs::new();
        vfs.write("/bin", vec![0, 159, 146, 150, 255], 7).unwrap();
        let json = serde_json::to_string(&vfs).unwrap();
        let restored: Vfs = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.read("/bin").unwrap(), vec![0, 159, 146, 150, 255]);
        assert_eq!(restored, vfs);
    }

    #[test]
    fn seed_file_creates_parents() {
        let mut vfs = Vfs::new();
        vfs.seed_file("/seeded/dir/config.json", b"{}".to_vec());
        assert!(vfs.exists("/seeded/dir"));
        assert_eq!(vfs.read_text("/seeded/dir/config.json").unwrap(), "{}");
    }
}
