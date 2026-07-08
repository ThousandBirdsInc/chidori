//! `package.json` reading and surgical editing.
//!
//! The manifest is kept as a raw `serde_json::Value` so fields we don't
//! understand (scripts, author, chidori-specific config, …) survive a
//! round-trip untouched.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

pub struct Manifest {
    path: PathBuf,
    value: Value,
}

impl Manifest {
    /// Load `dir/package.json`, or start a fresh minimal manifest if the file
    /// doesn't exist yet (it's written on the first `add`).
    pub fn load_or_default(dir: &Path) -> Result<Self> {
        let path = dir.join("package.json");
        let value = if path.is_file() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let value: Value = serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", path.display()))?;
            if !value.is_object() {
                bail!("{} is not a JSON object", path.display());
            }
            value
        } else {
            let name = dir
                .canonicalize()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .filter(|n| super::registry::validate_package_name(n).is_ok())
                .unwrap_or_else(|| "chidori-agent".to_string());
            json!({ "name": name, "private": true })
        };
        Ok(Self { path, value })
    }

    fn section(&self, key: &str) -> BTreeMap<String, String> {
        self.value
            .get(key)
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn dependencies(&self) -> BTreeMap<String, String> {
        self.section("dependencies")
    }

    pub fn dev_dependencies(&self) -> BTreeMap<String, String> {
        self.section("devDependencies")
    }

    /// All requested ranges (prod + dev). Dev wins on duplicate names, which
    /// matches npm's arborist behavior for the local tree.
    pub fn all_dependencies(&self) -> BTreeMap<String, String> {
        let mut all = self.dependencies();
        all.extend(self.dev_dependencies());
        all
    }

    pub fn set_dependency(&mut self, name: &str, range: &str, dev: bool) {
        let key = if dev {
            "devDependencies"
        } else {
            "dependencies"
        };
        let other = if dev {
            "dependencies"
        } else {
            "devDependencies"
        };
        // A package lives in exactly one section; moving it is intentional.
        if let Some(map) = self.value.get_mut(other).and_then(Value::as_object_mut) {
            map.remove(name);
        }
        let obj = self.value.as_object_mut().expect("validated as object");
        let section = obj
            .entry(key.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(map) = section.as_object_mut() {
            map.insert(name.to_string(), Value::String(range.to_string()));
            sort_map_in_place(map);
        }
    }

    /// Remove from both sections; true if it was present in either.
    pub fn remove_dependency(&mut self, name: &str) -> bool {
        let mut removed = false;
        for key in ["dependencies", "devDependencies"] {
            if let Some(map) = self.value.get_mut(key).and_then(Value::as_object_mut) {
                removed |= map.remove(name).is_some();
            }
        }
        removed
    }

    pub fn save(&self) -> Result<()> {
        let mut out = serde_json::to_string_pretty(&self.value).expect("manifest serializes");
        out.push('\n');
        std::fs::write(&self.path, out).with_context(|| format!("writing {}", self.path.display()))
    }
}

/// serde_json preserves insertion order by default; rewrite the map sorted so
/// dependency sections stay alphabetized like npm keeps them.
fn sort_map_in_place(map: &mut Map<String, Value>) {
    let mut entries: Vec<(String, Value)> = std::mem::take(map).into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    map.extend(entries);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_unknown_fields_and_sorts_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"x","customField":{"keep":true},"dependencies":{"zeta":"^1.0.0"}}"#,
        )
        .unwrap();
        let mut m = Manifest::load_or_default(dir.path()).unwrap();
        m.set_dependency("alpha", "^2.0.0", false);
        m.save().unwrap();

        let raw = std::fs::read_to_string(dir.path().join("package.json")).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["customField"]["keep"], Value::Bool(true));
        let deps: Vec<&String> = v["dependencies"].as_object().unwrap().keys().collect();
        assert_eq!(deps, ["alpha", "zeta"]);
    }

    #[test]
    fn add_moves_between_sections() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = Manifest::load_or_default(dir.path()).unwrap();
        m.set_dependency("p", "^1.0.0", false);
        m.set_dependency("p", "^1.0.0", true);
        assert!(m.dependencies().is_empty());
        assert_eq!(m.dev_dependencies().get("p").unwrap(), "^1.0.0");
    }

    #[test]
    fn remove_reports_presence() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = Manifest::load_or_default(dir.path()).unwrap();
        m.set_dependency("p", "^1.0.0", false);
        assert!(m.remove_dependency("p"));
        assert!(!m.remove_dependency("p"));
    }
}
