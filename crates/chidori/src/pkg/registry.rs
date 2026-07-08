//! npm registry client: abbreviated packument fetch + tarball download.
//!
//! Uses the `application/vnd.npm.install-v1+json` "abbreviated" packument
//! format, which carries only what installation needs (versions, dist-tags,
//! dependency maps, tarball URLs and integrity hashes) — typically 10-50x
//! smaller than the full document.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use tokio::sync::Mutex;

/// Env var overriding the registry base URL. Useful for corporate mirrors and
/// for hermetic tests that point at a local mock registry.
pub const REGISTRY_ENV: &str = "CHIDORI_NPM_REGISTRY";

const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";

/// Abbreviated packument: everything the registry knows about one package
/// name, in install-v1 form.
#[derive(Debug, Deserialize)]
pub struct Packument {
    #[serde(default, rename = "dist-tags")]
    pub dist_tags: HashMap<String, String>,
    #[serde(default)]
    pub versions: BTreeMap<String, PackageVersion>,
}

/// One published version of a package.
#[derive(Debug, Clone, Deserialize)]
pub struct PackageVersion {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "optionalDependencies")]
    pub optional_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "peerDependencies")]
    pub peer_dependencies: BTreeMap<String, String>,
    pub dist: Dist,
    #[serde(default)]
    pub deprecated: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Dist {
    pub tarball: String,
    /// Subresource-integrity string, e.g. `sha512-<base64>`. Present for
    /// everything published since ~2017.
    #[serde(default)]
    pub integrity: Option<String>,
    /// Legacy hex SHA-1, always present.
    #[serde(default)]
    pub shasum: Option<String>,
}

pub struct RegistryClient {
    base: String,
    http: reqwest::Client,
    packuments: Mutex<HashMap<String, Arc<Packument>>>,
}

impl RegistryClient {
    /// Client against `CHIDORI_NPM_REGISTRY` or the public npm registry.
    pub fn from_env() -> Result<Self> {
        let base = std::env::var(REGISTRY_ENV)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_REGISTRY.to_string());
        Self::new(base)
    }

    pub fn new(base: impl Into<String>) -> Result<Self> {
        let mut base = base.into();
        while base.ends_with('/') {
            base.pop();
        }
        let http = reqwest::Client::builder()
            .user_agent(concat!("chidori/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building registry HTTP client")?;
        Ok(Self {
            base,
            http,
            packuments: Mutex::new(HashMap::new()),
        })
    }

    /// Fetch (and memoize) the abbreviated packument for `name`.
    pub async fn packument(&self, name: &str) -> Result<Arc<Packument>> {
        validate_package_name(name)?;
        if let Some(hit) = self.packuments.lock().await.get(name) {
            return Ok(hit.clone());
        }
        // Scoped names keep their `/` encoded, matching npm client behavior.
        let url = format!("{}/{}", self.base, name.replace('/', "%2F"));
        let resp = self
            .http
            .get(&url)
            .header("accept", "application/vnd.npm.install-v1+json")
            .send()
            .await
            .with_context(|| format!("fetching registry metadata for `{name}`"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            bail!("package `{name}` not found in registry {}", self.base);
        }
        let resp = resp
            .error_for_status()
            .with_context(|| format!("registry error for `{name}`"))?;
        let packument: Packument = resp
            .json()
            .await
            .with_context(|| format!("parsing registry metadata for `{name}`"))?;
        let packument = Arc::new(packument);
        self.packuments
            .lock()
            .await
            .insert(name.to_string(), packument.clone());
        Ok(packument)
    }

    /// Download a package tarball, unverified. Verification happens in the
    /// store before extraction (`store::PackageStore::ensure`).
    pub async fn download_tarball(&self, url: &str) -> Result<Vec<u8>> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .with_context(|| format!("downloading {url}"))?
            .error_for_status()
            .with_context(|| format!("downloading {url}"))?;
        let bytes = resp
            .bytes()
            .await
            .with_context(|| format!("reading tarball body from {url}"))?;
        Ok(bytes.to_vec())
    }
}

/// Reject names that could escape into URL or filesystem tricks. Mirrors the
/// constraints of `validate-npm-package-name`, minus legacy grandfathered
/// names we have no reason to support.
pub fn validate_package_name(name: &str) -> Result<()> {
    let bad = |why: &str| anyhow!("invalid package name `{name}`: {why}");
    if name.is_empty() || name.len() > 214 {
        return Err(bad("must be 1-214 characters"));
    }
    let unscoped = if let Some(rest) = name.strip_prefix('@') {
        let (scope, unscoped) = rest
            .split_once('/')
            .ok_or_else(|| bad("scoped name must be @scope/name"))?;
        if scope.is_empty() || !scope.chars().all(is_name_char) {
            return Err(bad("bad scope segment"));
        }
        unscoped
    } else {
        name
    };
    if unscoped.is_empty() || !unscoped.chars().all(is_name_char) {
        return Err(bad(
            "only lowercase letters, digits, `-`, `_`, and `.` are allowed",
        ));
    }
    if unscoped.starts_with('.') || unscoped.starts_with('_') {
        return Err(bad("must not start with `.` or `_`"));
    }
    Ok(())
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_package_names() {
        assert!(validate_package_name("left-pad").is_ok());
        assert!(validate_package_name("@scope/pkg").is_ok());
        assert!(validate_package_name("@scope/pkg.js").is_ok());
        assert!(validate_package_name("").is_err());
        assert!(validate_package_name("UPPER").is_err());
        assert!(validate_package_name("../evil").is_err());
        assert!(validate_package_name("@/x").is_err());
        assert!(validate_package_name("@scope").is_err());
        assert!(validate_package_name(".dot").is_err());
        assert!(validate_package_name("a/b").is_err());
    }
}
