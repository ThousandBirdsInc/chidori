//! Node-style ESM module resolution for chidori agent code.
//!
//! Implements the subset of the Node ESM resolution algorithm that real-world
//! npm packages depend on, so generated agents can `import { x } from "pkg"`
//! and `import { y } from "pkg/sub"` the way bun/deno/node would resolve them.
//!
//! Covered:
//! - Relative and absolute specifiers (`./x`, `../x`, `/abs/x`)
//! - Bare package specifiers with `node_modules` walk-up (`pkg`, `@scope/pkg`)
//! - Package subpaths (`pkg/sub`)
//! - `package.json` `exports` field: string form, conditional object, subpath
//!   map, and one `*` pattern per key
//! - `main` field fallback when `exports` is absent
//! - `PACKAGE_SELF_RESOLVE` (`import "self" from inside the same package`)
//! - Extension probing (`.ts`, `.tsx`, `.js`, `.mjs`, `.cjs`, `.json`) and
//!   `index.*` fallback for relative imports and exports targets
//! - `node:` builtin specifiers, dispatched to a host-provided shim allowlist
//!
//! Deliberately out of scope (we don't need them for chidori-integrations and
//! adding them invites bugs):
//! - The `imports` field (`#x`)
//! - `typesVersions`
//! - CJS interop edge cases (`__esModule` markers, default-export reexport)
//! - URL specifiers (`https://`, `file://`)

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

/// Default ESM conditions matched against the `exports` field, in priority
/// order. `chidori` is a custom condition: packages that ship a chidori-aware
/// build can opt in via `"exports": { ".": { "chidori": "...", "import": "..." } }`.
pub const DEFAULT_CONDITIONS: &[&str] = &["chidori", "import", "module", "default"];

/// Same as DEFAULT_CONDITIONS but with `types` prepended, for the tsc-facing
/// resolution pass.
#[allow(dead_code)] // Staged for the tsc-facing resolution pass.
pub const TYPES_CONDITIONS: &[&str] = &["types", "chidori", "import", "module", "default"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolutionKind {
    /// Relative or absolute path resolved against the parent module.
    Relative,
    /// Bare specifier resolved through a `node_modules` package.
    Package {
        /// The package name (e.g. `@chidori-integrations/google-drive`).
        name: String,
        /// The subpath requested (e.g. `.` or `./sub`).
        subpath: String,
    },
    /// `node:` builtin that the host runtime provides via a shim.
    NodeBuiltin {
        /// The builtin name without the `node:` prefix (e.g. `process`).
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub kind: ResolutionKind,
    /// The on-disk path the bundler should load. For `NodeBuiltin` this is a
    /// virtual path under the resolution root (e.g. `<project>/__node_builtins__/process`)
    /// — the bundler keys modules by this path, and the shim layer registers
    /// sources under the same key.
    pub resolved_path: PathBuf,
}

/// A resolver scoped to a single project (== resolution root + condition set).
///
/// The resolver caches `package.json` parses so repeated lookups inside a
/// snapshot bundle don't re-read+parse the same files. The cache is
/// per-resolver so it doesn't outlive a bundle.
pub struct Resolver {
    project_root: PathBuf,
    conditions: Vec<String>,
    builtin_allowlist: Vec<String>,
    pkg_cache: RwLock<HashMap<PathBuf, Option<PackageJson>>>,
}

#[derive(Debug, Clone)]
struct PackageJson {
    dir: PathBuf,
    name: Option<String>,
    main: Option<String>,
    module: Option<String>,
    #[allow(dead_code)] // Parsed for the tsc-facing pass; not yet read.
    types: Option<String>,
    exports: Option<Value>,
}

impl Resolver {
    pub fn new(
        project_root: impl Into<PathBuf>,
        conditions: impl IntoIterator<Item = impl Into<String>>,
        builtin_allowlist: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            project_root: project_root.into(),
            conditions: conditions.into_iter().map(Into::into).collect(),
            builtin_allowlist: builtin_allowlist.into_iter().map(Into::into).collect(),
            pkg_cache: RwLock::new(HashMap::new()),
        }
    }

    #[allow(dead_code)] // Not yet wired into a call path; staged API.
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// Resolve `specifier` as if imported from `parent`. `parent` must be an
    /// absolute path (the agent entry or a previously resolved module).
    pub fn resolve(&self, specifier: &str, parent: &Path) -> Result<Resolution> {
        if specifier.is_empty() {
            bail!("empty import specifier from {}", parent.display());
        }

        if let Some(rest) = specifier.strip_prefix("node:") {
            return self.resolve_node_builtin(rest);
        }

        if is_relative_specifier(specifier) || specifier.starts_with('/') {
            let resolved = self.resolve_relative(specifier, parent)?;
            return Ok(Resolution {
                kind: ResolutionKind::Relative,
                resolved_path: resolved,
            });
        }

        if specifier.starts_with('#') {
            bail!(
                "`imports` field specifiers ({}) are not supported by the chidori resolver",
                specifier
            );
        }

        self.resolve_bare(specifier, parent)
    }

    fn resolve_node_builtin(&self, name: &str) -> Result<Resolution> {
        if !self.builtin_allowlist.iter().any(|b| b == name) {
            bail!(
                "node:{} is not provided by the chidori runtime (allowlist: {:?})",
                name,
                self.builtin_allowlist
            );
        }
        // Synthetic stable path. The shim layer registers a module under this
        // exact key when building the snapshot bundle.
        //
        // The path is a FIXED absolute root, deliberately independent of
        // `project_root`. A builtin shim may itself import another builtin
        // (e.g. the `fs` shim imports `node:buffer`); during that recursive
        // resolution the parent module is a synthetic builtin path, and the
        // re-derived `project_root` would otherwise differ — doubling the
        // `__node_builtins__` segment. A constant root keeps every builtin
        // resolving to the same key no matter where resolution starts.
        let path = Path::new("/")
            .join("__node_builtins__")
            .join(format!("{}.js", name));
        Ok(Resolution {
            kind: ResolutionKind::NodeBuiltin {
                name: name.to_string(),
            },
            resolved_path: path,
        })
    }

    fn resolve_relative(&self, specifier: &str, parent: &Path) -> Result<PathBuf> {
        let parent_dir = parent
            .parent()
            .ok_or_else(|| anyhow!("parent path {} has no directory", parent.display()))?;
        let raw = if specifier.starts_with('/') {
            PathBuf::from(specifier)
        } else {
            parent_dir.join(specifier)
        };
        let normalized = normalize_path(&raw);
        load_as_file_or_dir(&normalized)
            .ok_or_else(|| anyhow!("cannot resolve `{}` from {}", specifier, parent.display()))
    }

    fn resolve_bare(&self, specifier: &str, parent: &Path) -> Result<Resolution> {
        let (name, subpath) = split_package_specifier(specifier)?;

        // PACKAGE_SELF_RESOLVE: if any ancestor package.json declares this name
        // *and* exposes an `exports` field (Node only honors self-resolve when
        // exports is present), resolve through it.
        if let Some(pkg) = self.find_self_package(parent, &name)? {
            if pkg.exports.is_some() {
                let resolved = self
                    .resolve_package_subpath(&pkg, &subpath)
                    .with_context(|| {
                        format!("resolving self-import `{}` for package `{}`", subpath, name)
                    })?;
                return Ok(Resolution {
                    kind: ResolutionKind::Package { name, subpath },
                    resolved_path: resolved,
                });
            }
        }

        // Walk up node_modules from the parent's directory.
        let mut dir = parent.parent().map(Path::to_path_buf);
        while let Some(current) = dir {
            let candidate = current.join("node_modules").join(&name);
            if candidate.is_dir() {
                if let Some(pkg) = self.load_package_json(&candidate)? {
                    let resolved =
                        self.resolve_package_subpath(&pkg, &subpath)
                            .with_context(|| {
                                format!(
                                    "resolving `{}` from package `{}` at {}",
                                    subpath,
                                    name,
                                    candidate.display()
                                )
                            })?;
                    return Ok(Resolution {
                        kind: ResolutionKind::Package { name, subpath },
                        resolved_path: resolved,
                    });
                }
            }
            dir = current.parent().map(Path::to_path_buf);
            // Don't escape the project root: Node would happily walk to /,
            // but for an in-process bundler the project root is the boundary.
            if let Some(ref d) = dir {
                if !d.starts_with(&self.project_root) && d != &self.project_root {
                    break;
                }
            }
        }

        bail!(
            "cannot find package `{}` imported from {}",
            name,
            parent.display()
        )
    }

    fn find_self_package(&self, parent: &Path, name: &str) -> Result<Option<PackageJson>> {
        let mut dir = parent.parent().map(Path::to_path_buf);
        while let Some(current) = dir {
            if let Some(pkg) = self.load_package_json(&current)? {
                if pkg.name.as_deref() == Some(name) {
                    return Ok(Some(pkg));
                }
            }
            if current == self.project_root {
                break;
            }
            dir = current.parent().map(Path::to_path_buf);
        }
        Ok(None)
    }

    fn load_package_json(&self, dir: &Path) -> Result<Option<PackageJson>> {
        if let Some(cached) = self.pkg_cache.read().unwrap().get(dir) {
            return Ok(cached.clone());
        }
        let path = dir.join("package.json");
        let parsed = if path.is_file() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let value: Value = serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", path.display()))?;
            Some(PackageJson {
                dir: dir.to_path_buf(),
                name: value.get("name").and_then(|v| v.as_str()).map(String::from),
                main: value.get("main").and_then(|v| v.as_str()).map(String::from),
                module: value
                    .get("module")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                types: value
                    .get("types")
                    .or_else(|| value.get("typings"))
                    .and_then(|v| v.as_str())
                    .map(String::from),
                exports: value.get("exports").cloned(),
            })
        } else {
            None
        };
        self.pkg_cache
            .write()
            .unwrap()
            .insert(dir.to_path_buf(), parsed.clone());
        Ok(parsed)
    }

    fn resolve_package_subpath(&self, pkg: &PackageJson, subpath: &str) -> Result<PathBuf> {
        if let Some(exports) = &pkg.exports {
            return self
                .resolve_exports(&pkg.dir, exports, subpath)?
                .ok_or_else(|| {
                    anyhow!(
                        "package `{}` does not export `{}`",
                        pkg.name.as_deref().unwrap_or("<anon>"),
                        subpath
                    )
                });
        }
        // No exports field: fall back to main + LOAD_AS_FILE/LOAD_INDEX.
        if subpath == "." {
            let candidate_strs: Vec<&str> = pkg
                .module
                .as_deref()
                .into_iter()
                .chain(pkg.main.as_deref())
                .collect();
            for entry in candidate_strs {
                let path = pkg.dir.join(entry);
                if let Some(resolved) = load_as_file_or_dir(&path) {
                    return Ok(resolved);
                }
            }
            if let Some(resolved) = load_as_file_or_dir(&pkg.dir.join("index")) {
                return Ok(resolved);
            }
            bail!(
                "package `{}` has no main/module/index entry",
                pkg.name.as_deref().unwrap_or("<anon>")
            );
        }
        let stripped = subpath.strip_prefix("./").unwrap_or(subpath);
        let candidate = pkg.dir.join(stripped);
        load_as_file_or_dir(&candidate).ok_or_else(|| {
            anyhow!(
                "package `{}` has no subpath `{}` (no exports field)",
                pkg.name.as_deref().unwrap_or("<anon>"),
                subpath
            )
        })
    }

    fn resolve_exports(
        &self,
        pkg_dir: &Path,
        exports: &Value,
        subpath: &str,
    ) -> Result<Option<PathBuf>> {
        // Sugar: string, array, or "no key starts with '.'" all mean the same
        // shape for "." resolution.
        let sugar_for_dot = is_sugar_exports(exports);
        if subpath == "." {
            if sugar_for_dot {
                return self.resolve_exports_target(pkg_dir, exports, "");
            }
            if let Some(value) = exports.get(".") {
                return self.resolve_exports_target(pkg_dir, value, "");
            }
            return Ok(None);
        }
        if sugar_for_dot {
            return Ok(None);
        }
        let map = exports
            .as_object()
            .ok_or_else(|| anyhow!("exports field must be an object when matching subpaths"))?;
        // Exact match.
        if let Some(value) = map.get(subpath) {
            return self.resolve_exports_target(pkg_dir, value, "");
        }
        // Pattern match: pick the longest matching key, like Node does.
        let mut best: Option<(&String, &Value, String)> = None;
        for (key, value) in map {
            if !key.contains('*') {
                continue;
            }
            let (prefix, suffix) = key
                .split_once('*')
                .expect("key contains * so split_once succeeds");
            if subpath.starts_with(prefix) && subpath.ends_with(suffix) {
                let body_len = subpath.len() - prefix.len() - suffix.len();
                if subpath.len() < prefix.len() + suffix.len() {
                    continue;
                }
                let body = &subpath[prefix.len()..prefix.len() + body_len];
                if best.as_ref().is_none_or(|(best_key, _, _)| {
                    prefix.len() > best_key.split_once('*').unwrap().0.len()
                }) {
                    best = Some((key, value, body.to_string()));
                }
            }
        }
        if let Some((_, value, body)) = best {
            return self.resolve_exports_target(pkg_dir, value, &body);
        }
        Ok(None)
    }

    /// Walk a target value (string, conditional object, or array of fallbacks)
    /// and return the on-disk path. `pattern_body` is the captured `*` body
    /// substituted into the target.
    fn resolve_exports_target(
        &self,
        pkg_dir: &Path,
        target: &Value,
        pattern_body: &str,
    ) -> Result<Option<PathBuf>> {
        match target {
            Value::String(s) => {
                if !s.starts_with("./") {
                    bail!(
                        "exports target `{}` must be a relative path starting with ./",
                        s
                    );
                }
                let substituted = s.replace('*', pattern_body);
                let path = pkg_dir.join(substituted.trim_start_matches("./"));
                Ok(load_as_file_or_dir(&path).or(Some(path)))
            }
            Value::Object(map) => {
                // Walk our conditions in priority order rather than object key
                // order. serde_json's default Map is BTreeMap so key order is
                // alphabetical, which would silently invert Node's
                // source-order semantics. Consumers control the conditions
                // list, so priority-order traversal is both more predictable
                // and equivalent for well-formed `exports` blocks where the
                // intent is "first matching condition wins".
                for condition in &self.conditions {
                    if let Some(value) = map.get(condition) {
                        if let Some(resolved) =
                            self.resolve_exports_target(pkg_dir, value, pattern_body)?
                        {
                            return Ok(Some(resolved));
                        }
                    }
                }
                if let Some(value) = map.get("default") {
                    if let Some(resolved) =
                        self.resolve_exports_target(pkg_dir, value, pattern_body)?
                    {
                        return Ok(Some(resolved));
                    }
                }
                Ok(None)
            }
            Value::Array(items) => {
                for item in items {
                    if let Some(resolved) =
                        self.resolve_exports_target(pkg_dir, item, pattern_body)?
                    {
                        return Ok(Some(resolved));
                    }
                }
                Ok(None)
            }
            Value::Null => Ok(None),
            other => bail!("unsupported exports target shape: {}", other),
        }
    }
}

fn is_relative_specifier(specifier: &str) -> bool {
    specifier == "."
        || specifier == ".."
        || specifier.starts_with("./")
        || specifier.starts_with("../")
}

fn is_sugar_exports(exports: &Value) -> bool {
    match exports {
        Value::String(_) | Value::Array(_) => true,
        Value::Object(map) => !map.keys().any(|k| k.starts_with('.')),
        _ => false,
    }
}

fn split_package_specifier(specifier: &str) -> Result<(String, String)> {
    let (name, rest) = if let Some(rest) = specifier.strip_prefix('@') {
        // Scoped package: @scope/name[/subpath]
        let (scope, after_scope) = rest
            .split_once('/')
            .ok_or_else(|| anyhow!("invalid scoped specifier `{}`", specifier))?;
        match after_scope.split_once('/') {
            Some((pkg, sub)) => (format!("@{}/{}", scope, pkg), Some(sub.to_string())),
            None => (format!("@{}/{}", scope, after_scope), None),
        }
    } else {
        match specifier.split_once('/') {
            Some((pkg, sub)) => (pkg.to_string(), Some(sub.to_string())),
            None => (specifier.to_string(), None),
        }
    };
    let subpath = match rest {
        Some(s) => format!("./{}", s),
        None => ".".to_string(),
    };
    Ok((name, subpath))
}

const PROBE_EXTENSIONS: &[&str] = &["ts", "tsx", "js", "mjs", "cjs", "json"];

fn load_as_file_or_dir(path: &Path) -> Option<PathBuf> {
    load_as_file(path).or_else(|| load_as_index(path))
}

fn load_as_file(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    for ext in PROBE_EXTENSIONS {
        let mut candidate = path.as_os_str().to_owned();
        candidate.push(".");
        candidate.push(ext);
        let candidate = PathBuf::from(candidate);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn load_as_index(path: &Path) -> Option<PathBuf> {
    if !path.is_dir() {
        return None;
    }
    for ext in PROBE_EXTENSIONS {
        let candidate = path.join(format!("index.{}", ext));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn normalize_path(path: &Path) -> PathBuf {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    fn make_resolver(root: &Path) -> Resolver {
        Resolver::new(
            root,
            DEFAULT_CONDITIONS.iter().copied(),
            ["process".to_string(), "buffer".to_string()],
        )
    }

    #[test]
    fn resolves_relative_with_ts_extension() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(&root.join("agent.ts"), "");
        write(&root.join("tools/foo.ts"), "");
        let resolver = make_resolver(root);
        let res = resolver
            .resolve("./tools/foo", &root.join("agent.ts"))
            .unwrap();
        assert_eq!(res.kind, ResolutionKind::Relative);
        assert_eq!(res.resolved_path, root.join("tools/foo.ts"));
    }

    #[test]
    fn resolves_relative_index_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(&root.join("agent.ts"), "");
        write(&root.join("tools/foo/index.ts"), "");
        let resolver = make_resolver(root);
        let res = resolver
            .resolve("./tools/foo", &root.join("agent.ts"))
            .unwrap();
        assert_eq!(res.resolved_path, root.join("tools/foo/index.ts"));
    }

    #[test]
    fn resolves_bare_package_via_node_modules_with_exports_string() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(&root.join("agent.ts"), "");
        write(
            &root.join("node_modules/foo/package.json"),
            r#"{"name":"foo","exports":"./dist/index.js"}"#,
        );
        write(&root.join("node_modules/foo/dist/index.js"), "");
        let resolver = make_resolver(root);
        let res = resolver.resolve("foo", &root.join("agent.ts")).unwrap();
        match &res.kind {
            ResolutionKind::Package { name, subpath } => {
                assert_eq!(name, "foo");
                assert_eq!(subpath, ".");
            }
            other => panic!("expected Package, got {:?}", other),
        }
        assert_eq!(
            res.resolved_path,
            root.join("node_modules/foo/dist/index.js")
        );
    }

    #[test]
    fn resolves_bare_package_via_main_fallback() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(&root.join("agent.ts"), "");
        write(
            &root.join("node_modules/foo/package.json"),
            r#"{"name":"foo","main":"lib/main.js"}"#,
        );
        write(&root.join("node_modules/foo/lib/main.js"), "");
        let resolver = make_resolver(root);
        let res = resolver.resolve("foo", &root.join("agent.ts")).unwrap();
        assert_eq!(res.resolved_path, root.join("node_modules/foo/lib/main.js"));
    }

    #[test]
    fn resolves_scoped_package_with_subpath_and_conditions() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(&root.join("agent.ts"), "");
        write(
            &root.join("node_modules/@chidori/x/package.json"),
            r#"{
                "name": "@chidori/x",
                "exports": {
                    ".": { "import": "./dist/index.js", "default": "./dist/index.cjs" },
                    "./sub": { "chidori": "./dist/sub.chidori.js", "import": "./dist/sub.js" }
                }
            }"#,
        );
        write(&root.join("node_modules/@chidori/x/dist/index.js"), "");
        write(
            &root.join("node_modules/@chidori/x/dist/sub.chidori.js"),
            "",
        );
        write(&root.join("node_modules/@chidori/x/dist/sub.js"), "");
        let resolver = make_resolver(root);

        let entry = resolver
            .resolve("@chidori/x", &root.join("agent.ts"))
            .unwrap();
        assert_eq!(
            entry.resolved_path,
            root.join("node_modules/@chidori/x/dist/index.js")
        );
        let sub = resolver
            .resolve("@chidori/x/sub", &root.join("agent.ts"))
            .unwrap();
        // `chidori` condition wins because it's first in DEFAULT_CONDITIONS.
        assert_eq!(
            sub.resolved_path,
            root.join("node_modules/@chidori/x/dist/sub.chidori.js")
        );
    }

    #[test]
    fn resolves_subpath_pattern() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(&root.join("agent.ts"), "");
        write(
            &root.join("node_modules/foo/package.json"),
            r#"{
                "name": "foo",
                "exports": { "./feat/*": "./dist/feat/*.js" }
            }"#,
        );
        write(&root.join("node_modules/foo/dist/feat/alpha.js"), "");
        let resolver = make_resolver(root);
        let res = resolver
            .resolve("foo/feat/alpha", &root.join("agent.ts"))
            .unwrap();
        assert_eq!(
            res.resolved_path,
            root.join("node_modules/foo/dist/feat/alpha.js")
        );
    }

    #[test]
    fn node_modules_walk_up() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(&root.join("apps/agent.ts"), "");
        write(
            &root.join("node_modules/foo/package.json"),
            r#"{"name":"foo","exports":"./i.js"}"#,
        );
        write(&root.join("node_modules/foo/i.js"), "");
        let resolver = make_resolver(root);
        let res = resolver
            .resolve("foo", &root.join("apps/agent.ts"))
            .unwrap();
        assert_eq!(res.resolved_path, root.join("node_modules/foo/i.js"));
    }

    #[test]
    fn package_self_resolve() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("package.json"),
            r#"{"name":"my-pkg","exports":{".":"./src/index.ts","./util":"./src/util.ts"}}"#,
        );
        write(&root.join("src/index.ts"), "");
        write(&root.join("src/util.ts"), "");
        let resolver = make_resolver(root);
        let res = resolver
            .resolve("my-pkg/util", &root.join("src/index.ts"))
            .unwrap();
        assert_eq!(res.resolved_path, root.join("src/util.ts"));
    }

    #[test]
    fn rejects_unknown_node_builtin() {
        let dir = tempdir().unwrap();
        let resolver = make_resolver(dir.path());
        let err = resolver
            .resolve("node:fs", &dir.path().join("agent.ts"))
            .unwrap_err();
        assert!(err.to_string().contains("not provided"));
    }

    #[test]
    fn resolves_allowlisted_node_builtin() {
        let dir = tempdir().unwrap();
        let resolver = make_resolver(dir.path());
        let res = resolver
            .resolve("node:process", &dir.path().join("agent.ts"))
            .unwrap();
        assert!(matches!(
            res.kind,
            ResolutionKind::NodeBuiltin { ref name } if name == "process"
        ));
        assert!(res.resolved_path.ends_with("__node_builtins__/process.js"));
    }

    #[test]
    fn rejects_missing_package() {
        let dir = tempdir().unwrap();
        let resolver = make_resolver(dir.path());
        let err = resolver
            .resolve("nonexistent", &dir.path().join("agent.ts"))
            .unwrap_err();
        assert!(err.to_string().contains("cannot find package"));
    }

    #[test]
    fn split_package_specifier_basic() {
        assert_eq!(
            split_package_specifier("foo").unwrap(),
            ("foo".to_string(), ".".to_string())
        );
        assert_eq!(
            split_package_specifier("foo/bar").unwrap(),
            ("foo".to_string(), "./bar".to_string())
        );
        assert_eq!(
            split_package_specifier("@scope/foo").unwrap(),
            ("@scope/foo".to_string(), ".".to_string())
        );
        assert_eq!(
            split_package_specifier("@scope/foo/bar/baz").unwrap(),
            ("@scope/foo".to_string(), "./bar/baz".to_string())
        );
    }
}
