//! Runtime module loading, shared by the rust engine's linker host and the
//! tool loader.
//!
//! Given `(specifier, importer)` this resolves to an on-disk file, reads it,
//! and returns `(module key, ES module source)`:
//!
//! - **Relative imports from agent code** keep the strict project-rooted
//!   resolution (`resolve_relative_import`) that agent files always had.
//! - **Bare specifiers** (`zod`, `@scope/pkg/sub`) and **any import from
//!   inside `node_modules`** go through the full Node-style resolver
//!   (`resolver::Resolver`), so packages installed by `chidori add` load the
//!   way node/bun would load them.
//! - **JSON modules** become `export default <json>`.
//! - **Leaf CommonJS files** (no ESM syntax, uses `module.exports`) are
//!   wrapped so `import pkg from "cjs-pkg"` receives `module.exports` as the
//!   default export. `require()` is not emulated: a CJS file that calls it
//!   throws with a message steering toward ESM builds. This is deliberately
//!   minimal — real CJS graphs should ship an ESM build (most packages do).

use std::path::Path;

use crate::runtime::snapshot::TypeScriptImportPolicy;

use super::resolver::{Resolver, DEFAULT_CONDITIONS};
use super::transpile::{
    find_workspace_root, resolve_relative_import, transpile_module, TranspileOptions,
    NODE_BUILTIN_ALLOWLIST,
};

/// Resolve `specifier` from `importer_key`, read the module, and produce ES
/// module source. `node:` builtins and vendored packages must be handled by
/// the caller before this.
pub fn load_module_source(
    specifier: &str,
    importer_key: &str,
) -> std::result::Result<(String, String), String> {
    let importer = Path::new(importer_key);
    let in_node_modules = importer
        .components()
        .any(|c| c.as_os_str() == "node_modules");

    let resolved = if is_bare_specifier(specifier) || in_node_modules {
        // Full Node ESM resolution: node_modules walk-up, exports maps,
        // extension probing. Also used for relative imports *between package
        // files*, which may legitimately traverse `../` inside the package.
        let root = find_workspace_root(importer);
        let resolver = Resolver::new(
            root,
            DEFAULT_CONDITIONS.iter().copied(),
            NODE_BUILTIN_ALLOWLIST.iter().copied(),
        );
        resolver
            .resolve(specifier, importer)
            .map_err(|e| e.to_string())?
            .resolved_path
    } else {
        // Agent-code relative import: keep the historical strict behavior
        // (rooted at the importer's directory, no escaping).
        let dir = importer.parent().unwrap_or_else(|| Path::new("."));
        resolve_relative_import(importer, dir, specifier, 0).map_err(|e| e.to_string())?
    };

    let key = resolved.to_string_lossy().to_string();
    let src = std::fs::read_to_string(&resolved)
        .map_err(|e| format!("reading module {}: {e}", resolved.display()))?;

    if resolved.extension().and_then(|e| e.to_str()) == Some("json") {
        // Validate, then embed: a JSON document is a valid JS expression.
        serde_json::from_str::<serde::de::IgnoredAny>(&src)
            .map_err(|e| format!("parsing JSON module {}: {e}", resolved.display()))?;
        return Ok((key, format!("export default {src};\n")));
    }

    if in_dir_node_modules(&resolved) && looks_like_commonjs(&src) {
        return Ok((key, wrap_commonjs(&src)));
    }

    let js = transpile_module(
        &resolved,
        &src,
        &TranspileOptions {
            import_policy: TypeScriptImportPolicy::Node,
        },
    )
    .map_err(|e| e.to_string())?;
    Ok((key, js))
}

fn is_bare_specifier(specifier: &str) -> bool {
    !(specifier.starts_with("./")
        || specifier.starts_with("../")
        || specifier.starts_with('/')
        || specifier == "."
        || specifier == "..")
}

fn in_dir_node_modules(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "node_modules")
}

/// Heuristic: a file is CommonJS when it has no ESM statements but touches
/// the CJS module surface. Only ever applied to files under `node_modules`.
fn looks_like_commonjs(src: &str) -> bool {
    let has_esm = src.lines().any(|line| {
        let t = line.trim_start();
        (t.starts_with("import") || t.starts_with("export"))
            && matches!(
                t.as_bytes().get(6),
                Some(b' ' | b'{' | b'"' | b'\'' | b'*' | b'(')
            )
    });
    if has_esm {
        return false;
    }
    src.contains("module.exports") || src.contains("exports.") || src.contains("exports[")
}

/// Adapt a leaf CommonJS module to ESM: run the body with `module`/`exports`
/// in scope (and `this` bound to `exports`, which UMD wrappers rely on), then
/// re-export `module.exports` as the default export.
fn wrap_commonjs(src: &str) -> String {
    format!(
        "const module = {{ exports: {{}} }};\n\
         const exports = module.exports;\n\
         const require = (spec) => {{\n\
             throw new Error(\"Cannot require('\" + spec + \"'): chidori loads npm packages as ES modules and does not emulate CommonJS require(). Use a package (or entry point) that ships an ESM build.\");\n\
         }};\n\
         (function () {{\n{src}\n}}).call(module.exports);\n\
         export default module.exports;\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn bare_specifier_resolves_through_node_modules() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(&root.join("package.json"), r#"{"name":"proj"}"#);
        write(&root.join("agent.ts"), "");
        write(
            &root.join("node_modules/greeter/package.json"),
            r#"{"name":"greeter","exports":"./index.js"}"#,
        );
        write(
            &root.join("node_modules/greeter/index.js"),
            "export const hi = 1;\n",
        );
        let (key, src) =
            load_module_source("greeter", root.join("agent.ts").to_str().unwrap()).unwrap();
        assert!(key.ends_with("node_modules/greeter/index.js"));
        assert!(src.contains("export const hi"));
    }

    #[test]
    fn package_internal_relative_import_may_traverse_up() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(&root.join("package.json"), r#"{"name":"proj"}"#);
        write(&root.join("node_modules/pkg/lib/deep/a.js"), "");
        write(
            &root.join("node_modules/pkg/util.js"),
            "export default 1;\n",
        );
        let (key, _) = load_module_source(
            "../../util.js",
            root.join("node_modules/pkg/lib/deep/a.js")
                .to_str()
                .unwrap(),
        )
        .unwrap();
        assert!(key.ends_with("node_modules/pkg/util.js"));
    }

    #[test]
    fn leaf_commonjs_gets_default_export_wrapper() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(&root.join("package.json"), r#"{"name":"proj"}"#);
        write(&root.join("agent.ts"), "");
        write(
            &root.join("node_modules/ms/package.json"),
            r#"{"name":"ms","main":"./index.js"}"#,
        );
        write(
            &root.join("node_modules/ms/index.js"),
            "module.exports = function ms(v) { return v; };\n",
        );
        let (_, src) = load_module_source("ms", root.join("agent.ts").to_str().unwrap()).unwrap();
        assert!(src.contains("export default module.exports;"));
        assert!(src.contains("function ms"));
    }

    #[test]
    fn esm_package_files_are_not_wrapped() {
        assert!(!looks_like_commonjs(
            "import x from 'y';\nexport default x;\n"
        ));
        assert!(!looks_like_commonjs("export function exportsAll() {}\n"));
        assert!(looks_like_commonjs("module.exports = 1;\n"));
        assert!(looks_like_commonjs("exports.foo = 1;\n"));
    }

    #[test]
    fn json_modules_become_default_exports() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(&root.join("package.json"), r#"{"name":"proj"}"#);
        write(&root.join("agent.ts"), "");
        write(
            &root.join("node_modules/data/package.json"),
            r#"{"name":"data","exports":{"./config.json":"./config.json"}}"#,
        );
        write(&root.join("node_modules/data/config.json"), r#"{"a":1}"#);
        let (_, src) =
            load_module_source("data/config.json", root.join("agent.ts").to_str().unwrap())
                .unwrap();
        assert_eq!(src, "export default {\"a\":1};\n");
    }

    #[test]
    fn agent_relative_imports_stay_strict() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(&root.join("proj/agent.ts"), "");
        write(&root.join("secret.ts"), "export const s = 1;\n");
        let err = load_module_source("../secret", root.join("proj/agent.ts").to_str().unwrap())
            .unwrap_err();
        assert!(err.contains("escapes project root"), "got: {err}");
    }
}
