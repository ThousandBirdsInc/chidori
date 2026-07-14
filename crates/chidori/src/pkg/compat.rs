//! Post-install compatibility scan for `chidori add`.
//!
//! Chidori's embedded engine is not Node: CommonJS `require()` only works for
//! leaf modules, `node:` builtins are a small allowlisted set of shims, and
//! native addons can never load. None of that is visible at install time —
//! `chidori add somepkg` succeeds as pure data movement and the failure only
//! surfaces when the agent imports the package. This module closes that gap:
//! after materializing `node_modules`, the freshly added root packages are
//! scanned for the three known cliffs and a warning is printed for each, so
//! the author learns about an incompatible package at `add` time instead of
//! at first import.
//!
//! The scan is heuristic and bounded (metadata plus a capped source sweep);
//! it can miss dynamically-built specifiers, so it warns — it never fails the
//! install.

use std::collections::BTreeSet;
use std::path::Path;

use crate::runtime::typescript::transpile::NODE_BUILTIN_ALLOWLIST;

/// Every Node builtin module base name (the part before any `/` subpath).
/// Used to tell a builtin specifier apart from a package import; the subset
/// chidori actually provides is `NODE_BUILTIN_ALLOWLIST`.
const NODE_BUILTIN_BASES: &[&str] = &[
    "assert",
    "async_hooks",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "diagnostics_channel",
    "dns",
    "domain",
    "events",
    "fs",
    "http",
    "http2",
    "https",
    "inspector",
    "module",
    "net",
    "os",
    "path",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "repl",
    "stream",
    "string_decoder",
    "sys",
    "timers",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
];

/// Bounds for the source sweep, so `chidori add` stays fast on huge packages.
const MAX_SCANNED_FILES: usize = 400;
const MAX_FILE_BYTES: u64 = 512 * 1024;

/// Scan one installed package directory and return human-readable warnings
/// for anything that will not work under chidori's embedded engine.
pub fn check_package_compat(name: &str, pkg_dir: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    let manifest: serde_json::Value = std::fs::read_to_string(pkg_dir.join("package.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(serde_json::Value::Null);

    if let Some(reason) = native_addon_marker(pkg_dir, &manifest) {
        warnings.push(format!(
            "`{name}` {reason} — native addons cannot load in chidori's embedded \
             engine, so importing its native parts will fail at runtime"
        ));
    }

    if !has_esm_entry(pkg_dir, &manifest) {
        warnings.push(format!(
            "`{name}` ships no ES module build (no `type: \"module\"`, `exports` \
             import condition, `module` field, or `.mjs` entry). CommonJS support \
             is leaf-only: it will load ONLY if its entry never calls require() at \
             runtime. Prefer a package that ships ESM \
             (docs/package-management.md#compatibility)"
        ));
    }

    let missing = unsupported_builtin_imports(pkg_dir);
    if !missing.is_empty() {
        let list: Vec<&str> = missing.iter().map(String::as_str).collect();
        warnings.push(format!(
            "`{name}` references Node builtins the chidori runtime does not provide \
             ({}); imports of those modules will fail at runtime \
             (provided: {})",
            list.join(", "),
            NODE_BUILTIN_ALLOWLIST.join(", "),
        ));
    }

    warnings
}

/// Native-addon markers: a `binding.gyp`, `gypfile: true`, a dependency on the
/// usual prebuild loaders, or a shipped `.node` binary.
fn native_addon_marker(pkg_dir: &Path, manifest: &serde_json::Value) -> Option<String> {
    if pkg_dir.join("binding.gyp").exists() {
        return Some("contains a binding.gyp (node-gyp native build)".to_string());
    }
    if manifest.get("gypfile").and_then(|v| v.as_bool()) == Some(true) {
        return Some("declares `gypfile: true` (node-gyp native build)".to_string());
    }
    const NATIVE_LOADERS: &[&str] = &[
        "node-gyp",
        "node-gyp-build",
        "prebuild-install",
        "node-addon-api",
        "bindings",
        "node-pre-gyp",
        "@mapbox/node-pre-gyp",
    ];
    for section in ["dependencies", "peerDependencies", "optionalDependencies"] {
        if let Some(deps) = manifest.get(section).and_then(|v| v.as_object()) {
            for loader in NATIVE_LOADERS {
                if deps.contains_key(*loader) {
                    return Some(format!("depends on `{loader}` (native addon loader)"));
                }
            }
        }
    }
    if let Some(file) = find_file_with_extension(pkg_dir, "node", 0) {
        return Some(format!("ships a compiled `.node` binary ({file})"));
    }
    None
}

/// Does the package advertise any ES-module entry the resolver can prefer?
fn has_esm_entry(pkg_dir: &Path, manifest: &serde_json::Value) -> bool {
    if manifest.get("type").and_then(|v| v.as_str()) == Some("module") {
        return true;
    }
    if manifest.get("module").and_then(|v| v.as_str()).is_some() {
        return true;
    }
    if let Some(main) = manifest.get("main").and_then(|v| v.as_str()) {
        if main.ends_with(".mjs") {
            return true;
        }
    }
    if let Some(exports) = manifest.get("exports") {
        if exports_has_import_condition(exports) {
            return true;
        }
    }
    // A package with no manifest hints may still be import-only via .mjs files.
    find_file_with_extension(pkg_dir, "mjs", 0).is_some()
}

/// Recursively look for an `import` (or `module`) condition key, or any `.mjs`
/// target, inside an `exports` map.
fn exports_has_import_condition(exports: &serde_json::Value) -> bool {
    match exports {
        serde_json::Value::String(target) => target.ends_with(".mjs"),
        serde_json::Value::Object(map) => map.iter().any(|(key, value)| {
            key == "import" || key == "module" || exports_has_import_condition(value)
        }),
        serde_json::Value::Array(items) => items.iter().any(exports_has_import_condition),
        _ => false,
    }
}

/// Bounded sweep of the package's JS sources for imports/requires of Node
/// builtins outside the shim allowlist. Returns the sorted set of offending
/// specifiers (with any `node:` prefix stripped).
fn unsupported_builtin_imports(pkg_dir: &Path) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut scanned = 0usize;
    scan_dir(pkg_dir, &mut found, &mut scanned);
    found
}

fn scan_dir(dir: &Path, found: &mut BTreeSet<String>, scanned: &mut usize) {
    if *scanned >= MAX_SCANNED_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if *scanned >= MAX_SCANNED_FILES {
            return;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            // A nested node_modules belongs to a transitive dependency; it
            // gets its own scan when it is itself `add`ed.
            if path.file_name().and_then(|n| n.to_str()) != Some("node_modules") {
                scan_dir(&path, found, scanned);
            }
            continue;
        }
        let is_js = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("js" | "mjs" | "cjs")
        );
        if !is_js {
            continue;
        }
        if entry
            .metadata()
            .map(|m| m.len() > MAX_FILE_BYTES)
            .unwrap_or(true)
        {
            continue;
        }
        *scanned += 1;
        if let Ok(source) = std::fs::read_to_string(&path) {
            collect_unsupported_specifiers(&source, found);
        }
    }
}

/// Extract module specifiers from `require(...)`, `import ... from ...`, and
/// dynamic `import(...)` and record the Node builtins chidori doesn't shim.
fn collect_unsupported_specifiers(source: &str, found: &mut BTreeSet<String>) {
    const OPENERS: &[&str] = &[
        "require(\"",
        "require('",
        "from \"",
        "from '",
        "import(\"",
        "import('",
        "import \"",
        "import '",
    ];
    for opener in OPENERS {
        let quote = opener.as_bytes()[opener.len() - 1] as char;
        let mut rest = source;
        while let Some(idx) = rest.find(opener) {
            rest = &rest[idx + opener.len()..];
            let Some(end) = rest.find(quote) else { break };
            let spec = &rest[..end];
            rest = &rest[end..];
            let bare = spec.strip_prefix("node:").unwrap_or(spec);
            let base = bare.split('/').next().unwrap_or(bare);
            if NODE_BUILTIN_BASES.contains(&base) && !NODE_BUILTIN_ALLOWLIST.contains(&bare) {
                found.insert(bare.to_string());
            }
        }
    }
}

/// Find one file with the given extension anywhere under `dir` (bounded walk).
fn find_file_with_extension(dir: &Path, ext: &str, depth: usize) -> Option<String> {
    if depth > 4 {
        return None;
    }
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let file_type = entry.file_type().ok()?;
        if file_type.is_file() && path.extension().and_then(|e| e.to_str()) == Some(ext) {
            return Some(path.file_name()?.to_string_lossy().into_owned());
        }
        if file_type.is_dir() && path.file_name().and_then(|n| n.to_str()) != Some("node_modules") {
            if let Some(hit) = find_file_with_extension(&path, ext, depth + 1) {
                return Some(hit);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_pkg(tag: &str, manifest: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("chidori-compat-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("package.json"), manifest).unwrap();
        dir
    }

    #[test]
    fn clean_esm_package_yields_no_warnings() {
        let dir = temp_pkg(
            "esm",
            r#"{"name":"zed","version":"1.0.0","type":"module","main":"index.js"}"#,
        );
        std::fs::write(
            dir.join("index.js"),
            "import path from 'node:path';\nexport const x = 1;\n",
        )
        .unwrap();
        assert!(check_package_compat("zed", &dir).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn exports_import_condition_counts_as_esm() {
        let dir = temp_pkg(
            "exports",
            r#"{"name":"dualpkg","version":"1.0.0","exports":{".":{"import":"./index.mjs","require":"./index.cjs"}}}"#,
        );
        std::fs::write(dir.join("index.mjs"), "export const x = 1;\n").unwrap();
        std::fs::write(dir.join("index.cjs"), "module.exports = { x: 1 };\n").unwrap();
        assert!(check_package_compat("dualpkg", &dir).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cjs_only_package_warns() {
        let dir = temp_pkg(
            "cjs",
            r#"{"name":"oldpkg","version":"1.0.0","main":"index.js"}"#,
        );
        std::fs::write(dir.join("index.js"), "module.exports = 42;\n").unwrap();
        let warnings = check_package_compat("oldpkg", &dir);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("no ES module build"), "{warnings:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unsupported_builtin_imports_warn() {
        let dir = temp_pkg(
            "builtins",
            r#"{"name":"netpkg","version":"1.0.0","type":"module","main":"index.js"}"#,
        );
        std::fs::write(
            dir.join("index.js"),
            "import net from 'node:net';\nimport { Readable } from \"stream\";\nimport path from 'node:path';\nexport {};\n",
        )
        .unwrap();
        let warnings = check_package_compat("netpkg", &dir);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("net"), "{warnings:?}");
        assert!(warnings[0].contains("stream"), "{warnings:?}");
        // path IS allowlisted — it must not be flagged.
        assert!(!warnings[0].contains("(path"), "{warnings:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn native_addon_markers_warn() {
        let dir = temp_pkg(
            "native",
            r#"{"name":"fastlib","version":"1.0.0","type":"module","dependencies":{"node-gyp-build":"^4.0.0"}}"#,
        );
        let warnings = check_package_compat("fastlib", &dir);
        assert!(
            warnings.iter().any(|w| w.contains("node-gyp-build")),
            "{warnings:?}"
        );

        let dir2 = temp_pkg(
            "gyp",
            r#"{"name":"gyplib","version":"1.0.0","type":"module"}"#,
        );
        std::fs::write(dir2.join("binding.gyp"), "{}").unwrap();
        let warnings2 = check_package_compat("gyplib", &dir2);
        assert!(
            warnings2.iter().any(|w| w.contains("binding.gyp")),
            "{warnings2:?}"
        );

        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dir2);
    }

    #[test]
    fn shipped_node_binary_warns() {
        let dir = temp_pkg(
            "prebuilt",
            r#"{"name":"prebuilt","version":"1.0.0","type":"module"}"#,
        );
        std::fs::create_dir_all(dir.join("prebuilds/linux-x64")).unwrap();
        std::fs::write(dir.join("prebuilds/linux-x64/lib.node"), b"\x7fELF").unwrap();
        let warnings = check_package_compat("prebuilt", &dir);
        assert!(warnings.iter().any(|w| w.contains(".node")), "{warnings:?}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
