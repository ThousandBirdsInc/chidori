//! Content-addressed global package store + hardlink materialization.
//!
//! Every package version lives extracted under
//! `~/.chidori/cache/packages/<integrity-key>/`, keyed by its registry
//! integrity hash. A version is downloaded, verified, and extracted exactly
//! once per machine; projects get it materialized into `node_modules` via
//! hardlinks (falling back to copies across filesystems), so warm installs do
//! no network I/O and duplicate no file contents.
//!
//! Store entries are written atomically: extract into a temp sibling
//! directory, then `rename` into place. A concurrent install racing on the
//! same package loses the rename and simply uses the winner's entry.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use sha2::Digest as _;

/// Env var overriding the store location (tests, CI, shared caches).
pub const STORE_ENV: &str = "CHIDORI_PKG_CACHE_DIR";

/// What to verify a tarball against, in preference order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Integrity {
    /// `sha512-<base64>` subresource-integrity string from the registry.
    Sha512(Vec<u8>),
    /// Legacy hex SHA-1 `shasum` for pre-2017 publishes.
    Sha1(Vec<u8>),
}

/// Env var opting in to the collision-broken SHA-1 `shasum` fallback for
/// packages that publish no `sha512` integrity (pre-2017 publishes).
pub const ALLOW_SHA1_ENV: &str = "CHIDORI_PKG_ALLOW_SHA1";

impl Integrity {
    /// Prefer the SRI `integrity` field, fall back to the legacy shasum.
    ///
    /// SHA-1 has had practical collision attacks since 2017 (SHAttered), so
    /// the shasum fallback is refused unless `CHIDORI_PKG_ALLOW_SHA1=1`
    /// explicitly opts in — installing a legacy package then still verifies
    /// against the shasum rather than nothing at all.
    pub fn from_dist(integrity: Option<&str>, shasum: Option<&str>) -> Result<Self> {
        let allow_sha1 = std::env::var(ALLOW_SHA1_ENV).is_ok_and(|v| v.trim() == "1");
        Self::from_dist_with_options(integrity, shasum, allow_sha1)
    }

    /// [`Integrity::from_dist`] with the SHA-1 opt-in as an explicit argument,
    /// so tests can exercise both modes without racing on process env.
    pub fn from_dist_with_options(
        integrity: Option<&str>,
        shasum: Option<&str>,
        allow_sha1: bool,
    ) -> Result<Self> {
        if let Some(sri) = integrity {
            // SRI allows space-separated alternatives; take the strongest
            // sha512 entry if present.
            for candidate in sri.split_whitespace() {
                if let Some(b64) = candidate.strip_prefix("sha512-") {
                    let digest = base64::engine::general_purpose::STANDARD
                        .decode(b64)
                        .context("decoding sha512 integrity")?;
                    if digest.len() != 64 {
                        bail!("sha512 integrity has wrong digest length");
                    }
                    return Ok(Integrity::Sha512(digest));
                }
            }
        }
        if let Some(hex_sum) = shasum {
            if !allow_sha1 {
                bail!(
                    "registry entry publishes only a SHA-1 shasum, which is \
                     collision-broken and refused by default; set {ALLOW_SHA1_ENV}=1 \
                     to install this legacy package anyway"
                );
            }
            let digest = hex::decode(hex_sum.trim()).context("decoding sha1 shasum")?;
            if digest.len() != 20 {
                bail!("sha1 shasum has wrong digest length");
            }
            return Ok(Integrity::Sha1(digest));
        }
        bail!("registry entry has neither sha512 integrity nor sha1 shasum")
    }

    /// Filesystem-safe store key, e.g. `sha512-<hex>`.
    pub fn store_key(&self) -> String {
        match self {
            Integrity::Sha512(d) => format!("sha512-{}", hex::encode(d)),
            Integrity::Sha1(d) => format!("sha1-{}", hex::encode(d)),
        }
    }

    /// Verify `bytes` against this integrity. CPU-heavy; callers should run
    /// it via `spawn_blocking` so hashing never stalls the download executor.
    pub fn verify(&self, bytes: &[u8]) -> Result<()> {
        let ok = match self {
            Integrity::Sha512(expected) => {
                sha2::Sha512::digest(bytes).as_slice() == expected.as_slice()
            }
            Integrity::Sha1(expected) => {
                use sha1::Digest as _;
                sha1::Sha1::digest(bytes).as_slice() == expected.as_slice()
            }
        };
        if !ok {
            bail!(
                "integrity verification failed (expected {})",
                self.store_key()
            );
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct PackageStore {
    root: PathBuf,
}

impl PackageStore {
    /// Store at `CHIDORI_PKG_CACHE_DIR`, else `~/.chidori/cache/packages`.
    pub fn from_env() -> Result<Self> {
        if let Some(dir) = std::env::var_os(STORE_ENV).filter(|s| !s.is_empty()) {
            return Ok(Self::new(PathBuf::from(dir)));
        }
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .filter(|p| !p.is_empty())
            .ok_or_else(|| {
                anyhow!("cannot locate home directory for the package store; set {STORE_ENV}")
            })?;
        Ok(Self::new(
            PathBuf::from(home)
                .join(".chidori")
                .join("cache")
                .join("packages"),
        ))
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Directory holding the extracted contents for `integrity`, if cached.
    pub fn lookup(&self, integrity: &Integrity) -> Option<PathBuf> {
        let dir = self.root.join(integrity.store_key());
        dir.is_dir().then_some(dir)
    }

    /// Verify `tarball` against `integrity`, extract it, and move it into the
    /// store. Returns the store directory. No-op if already present.
    pub fn put_tarball(&self, integrity: &Integrity, tarball: &[u8]) -> Result<PathBuf> {
        let final_dir = self.root.join(integrity.store_key());
        if final_dir.is_dir() {
            return Ok(final_dir);
        }
        integrity.verify(tarball)?;

        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating store root {}", self.root.display()))?;
        // Temp sibling inside the store root so the final rename never
        // crosses filesystems.
        let tmp_path = self
            .root
            .join(format!(".extract-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&tmp_path).context("creating store temp dir")?;
        if let Err(e) = extract_npm_tarball(tarball, &tmp_path) {
            let _ = std::fs::remove_dir_all(&tmp_path);
            return Err(e);
        }

        match std::fs::rename(&tmp_path, &final_dir) {
            Ok(()) => Ok(final_dir),
            Err(_) if final_dir.is_dir() => {
                // Lost a race with a concurrent install of the same package.
                let _ = std::fs::remove_dir_all(&tmp_path);
                Ok(final_dir)
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp_path);
                Err(e).with_context(|| {
                    format!("moving package into store at {}", final_dir.display())
                })
            }
        }
    }

    /// Materialize a store entry into `dest` by hardlinking every file
    /// (copying when linking fails, e.g. across devices). `dest` must not
    /// already exist.
    pub fn materialize(&self, store_dir: &Path, dest: &Path) -> Result<()> {
        link_tree(store_dir, dest)
    }
}

fn link_tree(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            link_tree(&from, &to)?;
        } else if ty.is_file() && std::fs::hard_link(&from, &to).is_err() {
            std::fs::copy(&from, &to)
                .with_context(|| format!("copying {} -> {}", from.display(), to.display()))?;
        }
        // Symlinks and other special entries are skipped: extract_npm_tarball
        // never writes them, so anything else in the store is foreign.
    }
    Ok(())
}

/// Extract an npm package tarball (gzipped tar) into `dest`, stripping the
/// leading path component (`package/` by convention, but npm accepts any
/// single root directory). Only regular files and directories are written;
/// symlinks/hardlinks in a tarball are rejected as malicious.
fn extract_npm_tarball(tarball: &[u8], dest: &Path) -> Result<()> {
    let gz = flate2::read::GzDecoder::new(tarball);
    let mut archive = tar::Archive::new(gz);
    for entry in archive.entries().context("reading tarball")? {
        let mut entry = entry.context("reading tarball entry")?;
        let path = entry.path().context("tarball entry path")?.into_owned();
        let rel = strip_root_component(&path)
            .ok_or_else(|| anyhow!("tarball entry outside package root: {}", path.display()))?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(&rel);
        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                std::fs::create_dir_all(&out)?;
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                if let Some(parent) = out.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut buf = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut buf)?;
                std::fs::write(&out, &buf).with_context(|| format!("writing {}", out.display()))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(mode) = entry.header().mode() {
                        if mode & 0o111 != 0 {
                            let _ = std::fs::set_permissions(
                                &out,
                                std::fs::Permissions::from_mode(0o755),
                            );
                        }
                    }
                }
            }
            tar::EntryType::Link | tar::EntryType::Symlink => {
                bail!(
                    "refusing tarball containing a link entry: {}",
                    path.display()
                );
            }
            // pax headers, gnu extensions etc. — metadata, not content.
            _ => {}
        }
    }
    Ok(())
}

/// Drop the first path component and reject traversal or absolute paths.
fn strip_root_component(path: &Path) -> Option<PathBuf> {
    let mut components = path.components();
    match components.next()? {
        std::path::Component::Normal(_) => {}
        _ => return None,
    }
    let mut out = PathBuf::new();
    for c in components {
        match c {
            std::path::Component::Normal(part) => out.push(part),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a gzipped npm-style tarball in memory.
    pub(crate) fn make_tarball(files: &[(&str, &str)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (path, body) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, format!("package/{path}"), body.as_bytes())
                .unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    fn integrity_of(bytes: &[u8]) -> Integrity {
        Integrity::Sha512(sha2::Sha512::digest(bytes).to_vec())
    }

    #[test]
    fn put_verify_and_materialize_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = PackageStore::new(dir.path().join("store"));
        let tarball = make_tarball(&[
            ("package.json", r#"{"name":"x","version":"1.0.0"}"#),
            ("lib/index.js", "module.exports = 1;"),
        ]);
        let integrity = integrity_of(&tarball);

        assert!(store.lookup(&integrity).is_none());
        let stored = store.put_tarball(&integrity, &tarball).unwrap();
        assert!(stored.join("lib/index.js").is_file());
        assert_eq!(store.lookup(&integrity), Some(stored.clone()));

        let dest = dir.path().join("node_modules/x");
        store.materialize(&stored, &dest).unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("lib/index.js")).unwrap(),
            "module.exports = 1;"
        );
    }

    #[test]
    fn rejects_corrupted_tarball() {
        let dir = tempfile::tempdir().unwrap();
        let store = PackageStore::new(dir.path());
        let tarball = make_tarball(&[("package.json", "{}")]);
        let mut wrong = tarball.clone();
        wrong[10] ^= 0xff;
        let integrity = integrity_of(&tarball);
        let err = store.put_tarball(&integrity, &wrong).unwrap_err();
        assert!(err.to_string().contains("integrity verification failed"));
    }

    #[test]
    fn rejects_path_traversal() {
        assert_eq!(strip_root_component(Path::new("package/../../etc/x")), None);
        assert_eq!(strip_root_component(Path::new("/etc/passwd")), None);
        assert_eq!(
            strip_root_component(Path::new("package/a/b.js")),
            Some(PathBuf::from("a/b.js"))
        );
    }

    #[test]
    fn integrity_prefers_sha512_and_falls_back_to_shasum_when_opted_in() {
        let sri = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode([7u8; 64])
        );
        match Integrity::from_dist_with_options(Some(&sri), Some("aa".repeat(20).as_str()), false)
            .unwrap()
        {
            Integrity::Sha512(d) => assert_eq!(d, vec![7u8; 64]),
            other => panic!("expected sha512, got {other:?}"),
        }
        match Integrity::from_dist_with_options(None, Some("ab".repeat(20).as_str()), true).unwrap()
        {
            Integrity::Sha1(d) => assert_eq!(d.len(), 20),
            other => panic!("expected sha1, got {other:?}"),
        }
        assert!(Integrity::from_dist_with_options(None, None, true).is_err());
    }

    #[test]
    fn integrity_refuses_sha1_only_dist_by_default() {
        let err = Integrity::from_dist_with_options(None, Some("ab".repeat(20).as_str()), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("SHA-1"), "{err}");
        assert!(err.contains(ALLOW_SHA1_ENV), "{err}");
        // A dist with a usable sha512 entry is unaffected by the gate.
        let sri = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode([9u8; 64])
        );
        assert!(Integrity::from_dist_with_options(
            Some(&sri),
            Some("ab".repeat(20).as_str()),
            false
        )
        .is_ok());
    }
}
