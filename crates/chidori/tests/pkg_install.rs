//! End-to-end tests for `chidori add` / `install` / `remove` against a
//! hermetic in-process npm registry.
//!
//! The mock registry is a plain threaded HTTP server (no async) serving
//! abbreviated packuments and gzipped tarballs it builds in memory, so the
//! tests exercise the full pipeline — resolution, SHA-512 verification,
//! content-addressed store, hardlink materialization, hoisting, lockfile,
//! pruning — without any network.
//!
//! The pkg commands read `CHIDORI_NPM_REGISTRY` and `CHIDORI_PKG_CACHE_DIR`
//! from the environment, which is process-global; every test takes ENV_LOCK.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use base64::Engine as _;
use sha2::Digest as _;

use chidori::pkg::lockfile::{Lockfile, LOCKFILE_NAME};
use chidori::pkg::{cmd_add, cmd_install, cmd_remove};
use chidori::runtime::typescript::resolver::{Resolver, DEFAULT_CONDITIONS};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Serialize env-mutating tests and point the pkg commands at this test's
/// registry + store.
fn setup_env(registry_url: &str, store_dir: &Path) -> MutexGuard<'static, ()> {
    let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("CHIDORI_NPM_REGISTRY", registry_url);
    std::env::set_var("CHIDORI_PKG_CACHE_DIR", store_dir);
    guard
}

// ---------------------------------------------------------------------------
// Mock registry
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MockRegistryData {
    /// name -> version -> (deps, tarball bytes, corrupt_flag)
    packages: BTreeMap<String, BTreeMap<String, MockVersion>>,
}

struct MockVersion {
    deps: BTreeMap<String, String>,
    /// peer name -> (range, optional-in-peerDependenciesMeta).
    peers: BTreeMap<String, (String, bool)>,
    tarball: Vec<u8>,
    /// Serve bytes that don't match the advertised integrity.
    corrupt: bool,
}

struct MockRegistry {
    base: String,
    data: Arc<Mutex<MockRegistryData>>,
    shutdown: Arc<AtomicBool>,
}

impl MockRegistry {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let data: Arc<Mutex<MockRegistryData>> = Arc::default();
        let shutdown = Arc::new(AtomicBool::new(false));

        let thread_data = data.clone();
        let thread_base = base.clone();
        let thread_shutdown = shutdown.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                let response = handle(&thread_data.lock().unwrap(), &thread_base, &path);
                let _ = stream.write_all(&response);
            }
        });

        Self {
            base,
            data,
            shutdown,
        }
    }

    fn stop(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Unblock accept() with one last connection.
        let _ = std::net::TcpStream::connect(self.base.trim_start_matches("http://"));
    }

    /// Publish `name@version` with the given dependencies and a package.json
    /// + index.js generated to match.
    fn publish(&self, name: &str, version: &str, deps: &[(&str, &str)]) {
        self.publish_inner(name, version, deps, &[], false);
    }

    fn publish_corrupt(&self, name: &str, version: &str) {
        self.publish_inner(name, version, &[], &[], true);
    }

    /// Publish with `peerDependencies`; each peer is (name, range, optional),
    /// where `optional` marks it optional in `peerDependenciesMeta`.
    fn publish_with_peers(&self, name: &str, version: &str, peers: &[(&str, &str, bool)]) {
        self.publish_inner(name, version, &[], peers, false);
    }

    fn publish_inner(
        &self,
        name: &str,
        version: &str,
        deps: &[(&str, &str)],
        peers: &[(&str, &str, bool)],
        corrupt: bool,
    ) {
        let deps: BTreeMap<String, String> = deps
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect();
        let peers: BTreeMap<String, (String, bool)> = peers
            .iter()
            .map(|(n, r, opt)| (n.to_string(), (r.to_string(), *opt)))
            .collect();
        let manifest = serde_json::json!({
            "name": name,
            "version": version,
            "main": "index.js",
            "dependencies": deps,
        });
        let tarball = make_tarball(&[
            ("package.json", &manifest.to_string()),
            (
                "index.js",
                &format!("export default {:?};", format!("{name}@{version}")),
            ),
        ]);
        self.data
            .lock()
            .unwrap()
            .packages
            .entry(name.to_string())
            .or_default()
            .insert(
                version.to_string(),
                MockVersion {
                    deps,
                    peers,
                    tarball,
                    corrupt,
                },
            );
    }
}

fn handle(data: &MockRegistryData, base: &str, path: &str) -> Vec<u8> {
    if let Some(file) = path.strip_prefix("/tarballs/") {
        // /tarballs/<name>__<version>.tgz ('/' in scoped names sanitized to '+')
        let stem = file.trim_end_matches(".tgz").replace('+', "/");
        if let Some((name, version)) = stem.rsplit_once("__") {
            if let Some(v) = data.packages.get(name).and_then(|m| m.get(version)) {
                let mut body = v.tarball.clone();
                if v.corrupt {
                    // Flip a byte inside the gzip payload.
                    let last = body.len() - 1;
                    body[last] ^= 0xff;
                }
                return http_response("application/octet-stream", &body);
            }
        }
        return http_404();
    }

    let name = path.trim_start_matches('/').replace("%2F", "/");
    let Some(versions) = data.packages.get(&name) else {
        return http_404();
    };
    let latest = versions.keys().last().unwrap().clone();
    let versions_json: serde_json::Map<String, serde_json::Value> = versions
        .iter()
        .map(|(version, v)| {
            let integrity = format!(
                "sha512-{}",
                base64::engine::general_purpose::STANDARD
                    .encode(sha2::Sha512::digest(&v.tarball))
            );
            let mut vjson = serde_json::json!({
                "name": name,
                "version": version,
                "dependencies": v.deps,
                "dist": {
                    "tarball": format!("{base}/tarballs/{}__{version}.tgz", name.replace('/', "+")),
                    "integrity": integrity,
                }
            });
            if !v.peers.is_empty() {
                let ranges: serde_json::Map<String, serde_json::Value> = v
                    .peers
                    .iter()
                    .map(|(n, (r, _))| (n.clone(), serde_json::Value::String(r.clone())))
                    .collect();
                vjson["peerDependencies"] = ranges.into();
                let meta: serde_json::Map<String, serde_json::Value> = v
                    .peers
                    .iter()
                    .filter(|(_, (_, optional))| *optional)
                    .map(|(n, _)| (n.clone(), serde_json::json!({ "optional": true })))
                    .collect();
                if !meta.is_empty() {
                    vjson["peerDependenciesMeta"] = meta.into();
                }
            }
            (version.clone(), vjson)
        })
        .collect();
    let packument = serde_json::json!({
        "name": name,
        "dist-tags": { "latest": latest },
        "versions": versions_json,
    });
    http_response("application/json", packument.to_string().as_bytes())
}

fn http_response(content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

fn http_404() -> Vec<u8> {
    b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec()
}

fn make_tarball(files: &[(&str, &str)]) -> Vec<u8> {
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

fn installed_version(project: &Path, rel: &str) -> Option<String> {
    let raw = std::fs::read_to_string(project.join(rel).join("package.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Some(v["version"].as_str()?.to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn add_installs_transitive_deps_hoisted_and_writes_lockfile() {
    let registry = MockRegistry::start();
    registry.publish("apple", "1.0.0", &[("berry", "^1.0.0")]);
    registry.publish("berry", "1.0.0", &[]);
    registry.publish("berry", "1.1.0", &[]);

    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let _env = setup_env(&registry.base, &tmp.path().join("store"));

    cmd_add(&project, &["apple".to_string()], false).unwrap();

    // apple installed, berry hoisted to the top at the highest satisfying
    // version.
    assert_eq!(
        installed_version(&project, "node_modules/apple").as_deref(),
        Some("1.0.0")
    );
    assert_eq!(
        installed_version(&project, "node_modules/berry").as_deref(),
        Some("1.1.0")
    );

    // package.json got a caret range on the resolved version.
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(project.join("package.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["dependencies"]["apple"], "^1.0.0");

    // Lockfile: header + one line per package, in order.
    let lock_text = std::fs::read_to_string(project.join(LOCKFILE_NAME)).unwrap();
    let lines: Vec<&str> = lock_text.lines().collect();
    assert_eq!(lines.len(), 3, "header + 2 packages:\n{lock_text}");
    assert!(lines[0].contains("chidoriLockfile"));
    assert!(lines[1].contains(r#""name":"apple""#));
    assert!(lines[2].contains(r#""name":"berry""#));

    // The store is content-addressed: 2 package dirs.
    let store_entries = std::fs::read_dir(tmp.path().join("store")).unwrap().count();
    assert_eq!(store_entries, 2);

    registry.stop();
}

#[test]
fn version_conflicts_nest_and_resolve_like_node() {
    let registry = MockRegistry::start();
    // apple pins berry@1.0.0 exactly; the root wants berry@^1.1.0.
    registry.publish("apple", "1.0.0", &[("berry", "1.0.0")]);
    registry.publish("berry", "1.0.0", &[]);
    registry.publish("berry", "1.1.0", &[]);

    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let _env = setup_env(&registry.base, &tmp.path().join("store"));

    cmd_add(
        &project,
        &["apple".to_string(), "berry@^1.1.0".to_string()],
        false,
    )
    .unwrap();

    assert_eq!(
        installed_version(&project, "node_modules/berry").as_deref(),
        Some("1.1.0")
    );
    assert_eq!(
        installed_version(&project, "node_modules/apple/node_modules/berry").as_deref(),
        Some("1.0.0")
    );

    // The runtime's Node-style resolver sees exactly what Node would.
    std::fs::write(project.join("agent.ts"), "export {};").unwrap();
    let resolver = Resolver::new(
        &project,
        DEFAULT_CONDITIONS.iter().copied(),
        Vec::<String>::new(),
    );
    let from_root = resolver
        .resolve("berry", &project.join("agent.ts"))
        .unwrap();
    assert_eq!(
        from_root.resolved_path,
        project.join("node_modules/berry/index.js")
    );
    let from_apple = resolver
        .resolve("berry", &project.join("node_modules/apple/index.js"))
        .unwrap();
    assert_eq!(
        from_apple.resolved_path,
        project.join("node_modules/apple/node_modules/berry/index.js")
    );

    registry.stop();
}

#[test]
fn warm_install_is_fully_offline_from_the_store() {
    let registry = MockRegistry::start();
    registry.publish("apple", "1.0.0", &[("berry", "^1.0.0")]);
    registry.publish("berry", "1.2.0", &[]);

    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let first = tmp.path().join("first");
    std::fs::create_dir(&first).unwrap();
    let _env = setup_env(&registry.base, &store);

    cmd_add(&first, &["apple".to_string()], false).unwrap();

    // Second project reuses the manifest + lockfile (a fresh clone).
    let second = tmp.path().join("second");
    std::fs::create_dir(&second).unwrap();
    for file in ["package.json", LOCKFILE_NAME] {
        std::fs::copy(first.join(file), second.join(file)).unwrap();
    }

    // Kill the registry: the warm install must not need it.
    registry.stop();
    std::env::set_var("CHIDORI_NPM_REGISTRY", "http://127.0.0.1:1"); // unreachable

    cmd_install(&second, true).unwrap();
    assert_eq!(
        installed_version(&second, "node_modules/apple").as_deref(),
        Some("1.0.0")
    );
    assert_eq!(
        installed_version(&second, "node_modules/berry").as_deref(),
        Some("1.2.0")
    );

    // Hardlink materialization: same inode in both projects (same store file).
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let a = std::fs::metadata(first.join("node_modules/apple/index.js")).unwrap();
        let b = std::fs::metadata(second.join("node_modules/apple/index.js")).unwrap();
        assert_eq!(
            a.ino(),
            b.ino(),
            "warm install should hardlink from the store"
        );
    }
}

#[test]
fn remove_prunes_package_and_unreachable_transitives_offline() {
    let registry = MockRegistry::start();
    registry.publish("apple", "1.0.0", &[("berry", "^1.0.0")]);
    registry.publish("berry", "1.0.0", &[]);
    registry.publish("cherry", "2.0.0", &[]);

    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let _env = setup_env(&registry.base, &tmp.path().join("store"));

    cmd_add(
        &project,
        &["apple".to_string(), "cherry".to_string()],
        false,
    )
    .unwrap();
    assert!(project.join("node_modules/berry").is_dir());

    // Removal never needs the registry.
    registry.stop();
    std::env::set_var("CHIDORI_NPM_REGISTRY", "http://127.0.0.1:1");

    cmd_remove(&project, &["apple".to_string()]).unwrap();
    assert!(!project.join("node_modules/apple").exists());
    assert!(
        !project.join("node_modules/berry").exists(),
        "berry is unreachable after apple's removal and must be pruned"
    );
    assert!(project.join("node_modules/cherry").is_dir());

    let lock = Lockfile::load(&project.join(LOCKFILE_NAME)).unwrap();
    assert_eq!(lock.packages.len(), 1);
    assert!(lock.packages.keys().all(|(n, _)| n == "cherry"));
}

#[test]
fn corrupted_tarball_is_rejected_by_integrity_check() {
    let registry = MockRegistry::start();
    registry.publish_corrupt("evil", "1.0.0");

    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let _env = setup_env(&registry.base, &tmp.path().join("store"));

    let err = cmd_add(&project, &["evil".to_string()], false).unwrap_err();
    assert!(
        format!("{err:#}").contains("integrity verification failed"),
        "unexpected error: {err:#}"
    );
    // Nothing corrupt entered the store.
    assert!(
        !tmp.path().join("store").is_dir()
            || std::fs::read_dir(tmp.path().join("store"))
                .unwrap()
                .next()
                .is_none()
    );

    registry.stop();
}

#[test]
fn frozen_install_fails_on_manifest_drift() {
    let registry = MockRegistry::start();
    registry.publish("apple", "1.0.0", &[]);
    registry.publish("berry", "1.0.0", &[]);

    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let _env = setup_env(&registry.base, &tmp.path().join("store"));

    cmd_add(&project, &["apple".to_string()], false).unwrap();

    // Hand-edit package.json to request something the lockfile doesn't cover.
    let manifest_path = project.join("package.json");
    let mut manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    manifest["dependencies"]["berry"] = "^1.0.0".into();
    std::fs::write(&manifest_path, manifest.to_string()).unwrap();

    let err = cmd_install(&project, true).unwrap_err();
    assert!(format!("{err:#}").contains("out of sync"), "got: {err:#}");

    // Without --frozen the same state re-resolves and installs.
    cmd_install(&project, false).unwrap();
    assert_eq!(
        installed_version(&project, "node_modules/berry").as_deref(),
        Some("1.0.0")
    );

    registry.stop();
}

#[test]
fn scoped_packages_install_under_scope_dirs() {
    let registry = MockRegistry::start();
    registry.publish("@acme/utils", "0.3.0", &[]);

    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let _env = setup_env(&registry.base, &tmp.path().join("store"));

    cmd_add(&project, &["@acme/utils".to_string()], false).unwrap();
    assert_eq!(
        installed_version(&project, "node_modules/@acme/utils").as_deref(),
        Some("0.3.0")
    );

    // Removing it also drops the emptied scope directory.
    cmd_remove(&project, &["@acme/utils".to_string()]).unwrap();
    assert!(!project.join("node_modules/@acme").exists());

    registry.stop();
}

#[test]
fn unsupported_manifest_deps_are_skipped_per_dependency() {
    let registry = MockRegistry::start();
    registry.publish("apple", "1.0.0", &[]);

    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let _env = setup_env(&registry.base, &tmp.path().join("store"));

    // A monorepo-style manifest: one file: dep (e.g. local SDK types) that
    // another tool materialized into node_modules as a real directory.
    std::fs::write(
        project.join("package.json"),
        r#"{"name":"demo","private":true,"devDependencies":{"local-types":"file:../sdk"}}"#,
    )
    .unwrap();
    let local = project.join("node_modules/local-types");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(
        local.join("package.json"),
        r#"{"name":"local-types","version":"0.0.0"}"#,
    )
    .unwrap();

    // add / install / remove all proceed despite the file: dep...
    cmd_add(&project, &["apple".to_string()], false).unwrap();
    assert_eq!(
        installed_version(&project, "node_modules/apple").as_deref(),
        Some("1.0.0")
    );
    cmd_install(&project, false).unwrap();
    cmd_remove(&project, &["apple".to_string()]).unwrap();
    assert!(!project.join("node_modules/apple").exists());

    // ...the manifest entry survives verbatim, and the unmanaged
    // node_modules entry is never pruned.
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(project.join("package.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["devDependencies"]["local-types"], "file:../sdk");
    assert!(
        project.join("node_modules/local-types").is_dir(),
        "unmanaged file: dep must survive pruning"
    );

    // Explicitly *requesting* an unsupported form is still a hard error.
    let err = cmd_add(&project, &["thing@file:../elsewhere".to_string()], false)
        .unwrap_err()
        .to_string();
    assert!(err.contains("file dependencies are not supported"), "{err}");

    registry.stop();
}

#[test]
fn add_writes_manifest_with_name_before_dependencies() {
    let registry = MockRegistry::start();
    registry.publish("apple", "1.0.0", &[]);

    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let _env = setup_env(&registry.base, &tmp.path().join("store"));

    // Start from a manifest whose keys would alphabetize badly ("dependencies"
    // sorts before "name"); after `add` the file must keep npm's conventional
    // top-level order with `name` first.
    std::fs::write(
        project.join("package.json"),
        r#"{"name":"demo","version":"0.1.0","private":true}"#,
    )
    .unwrap();
    cmd_add(&project, &["apple".to_string()], false).unwrap();

    let raw = std::fs::read_to_string(project.join("package.json")).unwrap();
    let pos = |key: &str| {
        raw.find(&format!("\"{key}\""))
            .unwrap_or_else(|| panic!("missing key {key}:\n{raw}"))
    };
    assert!(
        pos("name") < pos("dependencies"),
        "`name` must stay above `dependencies`:\n{raw}"
    );
    assert!(pos("version") < pos("dependencies"), "{raw}");
    assert!(pos("name") < pos("version"), "{raw}");

    registry.stop();
}

#[test]
fn peer_warnings_skip_optional_and_typescript_peers() {
    let registry = MockRegistry::start();
    // Like valibot: a `typescript` peer, plus one optional and one required
    // peer. Only the required non-typescript peer should warn.
    registry.publish_with_peers(
        "valibot-like",
        "1.0.0",
        &[
            ("typescript", ">=5", false),
            ("softpeer", "^1.0.0", true),
            ("hardpeer", "^2.0.0", false),
        ],
    );

    let client = chidori::pkg::registry::RegistryClient::new(registry.base.clone()).unwrap();
    let root_deps: BTreeMap<String, String> =
        [("valibot-like".to_string(), "^1.0.0".to_string())].into();
    let resolution = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(chidori::pkg::resolve::resolve(
            &client,
            &root_deps,
            &std::collections::HashMap::new(),
        ))
        .unwrap();

    let peer_warnings: Vec<&String> = resolution
        .warnings
        .iter()
        .filter(|w| w.contains("unmet peer dependency"))
        .collect();
    assert_eq!(
        peer_warnings.len(),
        1,
        "exactly the required non-typescript peer should warn: {:?}",
        resolution.warnings
    );
    assert!(peer_warnings[0].contains("hardpeer"), "{peer_warnings:?}");
    assert!(
        !resolution.warnings.iter().any(|w| w.contains("softpeer")),
        "optional peer must not warn: {:?}",
        resolution.warnings
    );
    assert!(
        !resolution.warnings.iter().any(|w| w.contains("typescript")),
        "typescript peer must not warn: {:?}",
        resolution.warnings
    );

    registry.stop();
}
