use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn chidori_bin() -> &'static str {
    env!("CARGO_BIN_EXE_chidori")
}

fn temp_project(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "chidori-cli-ts-{name}-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn unique_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        .to_string()
}

fn run_chidori(args: &[&str], cwd: &Path) -> Output {
    Command::new(chidori_bin())
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap()
}

fn run_chidori_with_env(args: &[&str], cwd: &Path, envs: &[(&str, &Path)]) -> Output {
    let mut command = Command::new(chidori_bin());
    command.args(args).current_dir(cwd);
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn run_chidori_with_str_env(args: &[&str], cwd: &Path, envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(chidori_bin());
    command.args(args).current_dir(cwd);
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "command unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cli_check_accepts_typescript_agent() {
    let dir = temp_project("check");
    let agent = dir.join("agent.ts");
    fs::write(
        &agent,
        r#"
            export async function agent(input, chidori) {
                await chidori.log("checking", { ok: true });
                return { ok: true };
            }
        "#,
    )
    .unwrap();

    let output = run_chidori(&["check", agent.to_str().unwrap()], &dir);
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("OK:"));

    fs::remove_dir_all(dir).ok();
}

#[test]
fn cli_check_rejects_legacy_starlark_agent() {
    let dir = temp_project("reject-star");
    let agent = dir.join("agent.star");
    fs::write(
        &agent,
        r#"
            def agent():
                return {"ok": True}
        "#,
    )
    .unwrap();

    let output = run_chidori(&["check", agent.to_str().unwrap()], &dir);
    assert_failure(&output);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("TypeScript `.ts` agents"));

    fs::remove_dir_all(dir).ok();
}

#[test]
fn cli_run_typescript_agent_outputs_json_and_persists_snapshot_manifest() {
    let dir = temp_project("run");
    let agent = dir.join("agent.ts");
    fs::write(
        &agent,
        r#"
            export async function agent(input, chidori) {
                const name = input.name ?? "world";
                await chidori.log("hello", { name });
                return { greeting: "Hello, " + name + "!" };
            }
        "#,
    )
    .unwrap();

    let output = run_chidori(
        &["run", agent.to_str().unwrap(), "--input", "name=CLI"],
        &dir,
    );
    assert_success(&output);
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["greeting"], "Hello, CLI!");

    let runs_dir = dir.join(".chidori").join("runs");
    let mut run_dirs = fs::read_dir(&runs_dir).unwrap();
    let run_dir = run_dirs.next().unwrap().unwrap().path();
    assert!(run_dir.join("checkpoint.json").exists());
    assert!(run_dir.join("runtime.snapshot.json").exists());
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(run_dir.join("runtime.snapshot.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["abi"]["engine_fork"], "chidori-quickjs");
    assert_eq!(manifest["snapshot_file"], "runtime.snapshot");

    fs::remove_dir_all(dir).ok();
}

#[test]
fn cli_run_discovers_local_typescript_tool_directory_for_direct_tool_calls() {
    let dir = temp_project("run-tool-dir");
    let tools_dir = dir.join("tools");
    fs::create_dir_all(&tools_dir).unwrap();
    let agent = dir.join("agent.ts");
    fs::write(
        &agent,
        r#"
            export async function agent(input, chidori) {
                const searchResults = await chidori.tool("web_search", {
                    query: "latest developments " + input.topic,
                });
                return { searchResults };
            }
        "#,
    )
    .unwrap();
    fs::write(
        tools_dir.join("web_search.ts"),
        r#"
            export const tool = {
                name: "web_search",
                description: "Search the web for a short query.",
                parameters: {
                    type: "object",
                    properties: {
                        query: { type: "string", description: "Search query" },
                    },
                    required: ["query"],
                },
            };

            export async function run(args, chidori) {
                await chidori.log("web_search", { query: args.query });
                return {
                    query: args.query,
                    results: [
                        { title: "Chidori tools", url: "https://example.test/chidori-tools" },
                    ],
                };
            }
        "#,
    )
    .unwrap();

    let output = run_chidori(
        &[
            "run",
            agent.to_str().unwrap(),
            "--input",
            r#"{"topic":"chidori"}"#,
        ],
        &dir,
    );
    assert_success(&output);
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        stdout["searchResults"]["query"],
        "latest developments chidori"
    );
    assert_eq!(
        stdout["searchResults"]["results"][0]["title"],
        "Chidori tools"
    );

    fs::remove_dir_all(dir).ok();
}

#[test]
fn cli_stream_typescript_agent_emits_call_and_done_events() {
    let dir = temp_project("stream");
    let agent = dir.join("agent.ts");
    fs::write(
        &agent,
        r#"
            export async function agent(input, chidori) {
                await chidori.log("streaming", { value: input.value });
                return { value: input.value };
            }
        "#,
    )
    .unwrap();

    let output = run_chidori(
        &[
            "run",
            agent.to_str().unwrap(),
            "--stream",
            "--input",
            r#"{"value": 42}"#,
        ],
        &dir,
    );
    assert_success(&output);
    let events: Vec<serde_json::Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(events
        .iter()
        .any(|event| event["type"] == "call" && event["record"]["function"] == "log"));
    assert!(events
        .iter()
        .any(|event| event["type"] == "done" && event["output"]["value"] == 42));

    fs::remove_dir_all(dir).ok();
}

#[test]
fn cli_stream_workspace_binding_writes_real_disk_manifest() {
    let dir = temp_project("stream-workspace");
    let workspace = dir.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let agent = dir.join("agent.ts");
    fs::write(
        &agent,
        r#"
            export async function agent(input, chidori) {
                await chidori.workspace.write("agent.ts", "export default {};", {
                    language: "typescript",
                });
                const files = await chidori.workspace.list({ completeOnly: true });
                return {
                    content: await chidori.workspace.read("agent.ts"),
                    files,
                    manifest: await chidori.workspace.manifest(),
                };
            }
        "#,
    )
    .unwrap();

    let output = run_chidori_with_env(
        &[
            "run",
            agent.to_str().unwrap(),
            "--stream",
            "--input",
            r#"{}"#,
        ],
        &dir,
        &[("CHIDORI_WORKSPACE_ROOT", &workspace)],
    );
    assert_success(&output);
    assert_eq!(
        fs::read_to_string(workspace.join("agent.ts")).unwrap(),
        "export default {};"
    );
    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".generation/manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["files"]["agent.ts"]["status"], "complete");

    let events: Vec<serde_json::Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(events
        .iter()
        .any(|event| event["type"] == "call" && event["record"]["function"] == "workspace"));
    assert!(events.iter().any(|event| {
        event["type"] == "done"
            && event["output"]["content"] == "export default {};"
            && event["output"]["files"][0]["path"] == "agent.ts"
    }));

    fs::remove_dir_all(dir).ok();
}

#[test]
fn cli_workspace_write_denied_by_policy_is_blocked() {
    let dir = temp_project("workspace-policy-deny");
    let workspace = dir.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let agent = dir.join("agent.ts");
    fs::write(
        &agent,
        r#"
            export async function agent(input, chidori) {
                await chidori.workspace.write("secret.ts", "export default {};");
                return { ok: true };
            }
        "#,
    )
    .unwrap();

    // A restrictive profile that denies workspace writes. Before workspace
    // effects were routed through the policy gate, this rule had no effect and
    // the write went through unconditionally.
    let policy = r#"{
        "default": "always_allow",
        "rules": [
            { "target": "workspace:write", "decision": "never_allow", "reason": "read-only profile" }
        ]
    }"#;

    let output = run_chidori_with_str_env(
        &["run", agent.to_str().unwrap(), "--input", r#"{}"#],
        &dir,
        &[
            ("CHIDORI_WORKSPACE_ROOT", workspace.to_str().unwrap()),
            ("CHIDORI_POLICY", policy),
        ],
    );

    assert_failure(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("workspace:write") && stderr.contains("denied"),
        "expected a workspace:write policy denial, got stderr:\n{stderr}"
    );
    // The deny must happen before the effect runs: no file on disk.
    assert!(
        !workspace.join("secret.ts").exists(),
        "denied workspace.write must not have touched the disk"
    );

    fs::remove_dir_all(dir).ok();
}

#[test]
fn cli_workspace_read_allowed_while_write_denied() {
    let dir = temp_project("workspace-policy-read-ok");
    let workspace = dir.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    // Seed a file the agent is allowed to read.
    fs::write(workspace.join("seed.txt"), "hello").unwrap();
    let agent = dir.join("agent.ts");
    fs::write(
        &agent,
        r#"
            export async function agent(input, chidori) {
                return { content: await chidori.workspace.read("seed.txt") };
            }
        "#,
    )
    .unwrap();

    // Deny only writes; reads fall through to the AlwaysAllow default.
    let policy = r#"{
        "default": "always_allow",
        "rules": [
            { "target": "workspace:write", "decision": "never_allow" }
        ]
    }"#;

    let output = run_chidori_with_str_env(
        &["run", agent.to_str().unwrap(), "--input", r#"{}"#],
        &dir,
        &[
            ("CHIDORI_WORKSPACE_ROOT", workspace.to_str().unwrap()),
            ("CHIDORI_POLICY", policy),
        ],
    );

    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hello"),
        "expected the allowed workspace.read to return its content, got:\n{stdout}"
    );

    fs::remove_dir_all(dir).ok();
}

#[test]
fn cli_untrusted_profile_denies_workspace_write() {
    let dir = temp_project("untrusted-workspace-write");
    let workspace = dir.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let agent = dir.join("agent.ts");
    fs::write(
        &agent,
        r#"
            export async function agent(input, chidori) {
                await chidori.workspace.write("agent.ts", "export default {};", {
                    language: "typescript",
                });
                return { ok: true };
            }
        "#,
    )
    .unwrap();

    let output = run_chidori_with_str_env(
        &["run", agent.to_str().unwrap(), "--input", r#"{}"#],
        &dir,
        &[
            ("CHIDORI_POLICY_PROFILE", "untrusted"),
            ("CHIDORI_WORKSPACE_ROOT", workspace.to_str().unwrap()),
        ],
    );
    assert_failure(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("workspace:write") && stderr.contains("denied"),
        "expected workspace:write denial, got stderr:\n{stderr}"
    );
    // The denied write must not have touched disk.
    assert!(!workspace.join("agent.ts").exists());

    fs::remove_dir_all(dir).ok();
}

#[test]
fn cli_untrusted_profile_allows_read_only_workspace() {
    let dir = temp_project("untrusted-workspace-read");
    let workspace = dir.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let agent = dir.join("agent.ts");
    // Read-only introspection is on the untrusted allowlist, so listing must
    // succeed even under the deny-by-default profile.
    fs::write(
        &agent,
        r#"
            export async function agent(input, chidori) {
                const files = await chidori.workspace.list({ completeOnly: true });
                return { count: files.length };
            }
        "#,
    )
    .unwrap();

    let output = run_chidori_with_str_env(
        &["run", agent.to_str().unwrap(), "--input", r#"{}"#],
        &dir,
        &[
            ("CHIDORI_POLICY_PROFILE", "untrusted"),
            ("CHIDORI_WORKSPACE_ROOT", workspace.to_str().unwrap()),
        ],
    );
    assert_success(&output);

    fs::remove_dir_all(dir).ok();
}

#[test]
fn cli_untrusted_flag_overrides_permissive_policy_env() {
    let dir = temp_project("untrusted-flag-precedence");
    let workspace = dir.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let agent = dir.join("agent.ts");
    fs::write(
        &agent,
        r#"
            export async function agent(input, chidori) {
                await chidori.workspace.write("out.txt", "data");
                return { ok: true };
            }
        "#,
    )
    .unwrap();

    // --untrusted must win over an explicitly permissive CHIDORI_POLICY:
    // the flag is the operator's last word, env vars are ambient config.
    let permissive = r#"{ "default": "always_allow", "rules": [] }"#;
    let output = run_chidori_with_str_env(
        &[
            "run",
            agent.to_str().unwrap(),
            "--untrusted",
            "--input",
            r#"{}"#,
        ],
        &dir,
        &[
            ("CHIDORI_POLICY", permissive),
            ("CHIDORI_WORKSPACE_ROOT", workspace.to_str().unwrap()),
        ],
    );
    assert_failure(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("workspace:write") && stderr.contains("denied"),
        "expected --untrusted to deny workspace:write despite permissive env policy, got stderr:\n{stderr}"
    );
    assert!(!workspace.join("out.txt").exists());

    fs::remove_dir_all(dir).ok();
}

#[test]
fn cli_stream_accepts_multiline_typed_agent_signature() {
    let dir = temp_project("stream-multiline-types");
    let agent = dir.join("agent.ts");
    fs::write(
        &agent,
        r#"
            import type { Chidori } from "chidori";

            export async function agent(
                input: { name?: string },
                chidori: Chidori,
            ) {
                const name = input.name ?? "world";
                await chidori.log("hello", { name });
                return { greeting: "Hello, " + name + "!" };
            }
        "#,
    )
    .unwrap();

    let output = run_chidori(
        &[
            "run",
            agent.to_str().unwrap(),
            "--stream",
            "--input",
            r#"{"name":"Ada"}"#,
        ],
        &dir,
    );
    assert_success(&output);
    let events: Vec<serde_json::Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(events
        .iter()
        .any(|event| event["type"] == "done" && event["output"]["greeting"] == "Hello, Ada!"));

    fs::remove_dir_all(dir).ok();
}

#[test]
fn cli_stream_failure_done_event_includes_full_error_chain() {
    let dir = temp_project("stream-failure");
    let agent = dir.join("agent.ts");
    fs::write(
        &agent,
        r#"
            export async function agent(input, chidori) {
                const broken = ;
                return { broken };
            }
        "#,
    )
    .unwrap();

    let output = run_chidori(
        &[
            "run",
            agent.to_str().unwrap(),
            "--stream",
            "--input",
            r#"{}"#,
        ],
        &dir,
    );
    assert_failure(&output);

    let events: Vec<serde_json::Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let done = events
        .iter()
        .find(|event| event["type"] == "done")
        .expect("missing done event");
    let error = done["error"].as_str().unwrap_or_default();
    // oxc surfaces malformed agent code as a parse error before we ever reach
    // QuickJS; the done event still has to name what failed and where so the
    // CLI user can find the broken file.
    assert!(error.contains("agent.ts"), "{error}");
    assert!(error.contains("TypeScript parse error"), "{error}");

    fs::remove_dir_all(dir).ok();
}

#[test]
fn cli_tools_lists_typescript_tools_and_ignores_starlark_files() {
    let dir = temp_project("tools");
    let tools_dir = dir.join("tools");
    fs::create_dir_all(&tools_dir).unwrap();
    fs::write(
        tools_dir.join("web_search.ts"),
        r#"
            export const tool = {
                name: "web_search",
                description: "Search the web.",
                parameters: {
                    type: "object",
                    properties: { query: { type: "string" } },
                    required: ["query"],
                },
            };
            export async function run(args, chidori) {
                await chidori.log("search", args);
                return { results: [] };
            }
        "#,
    )
    .unwrap();
    fs::write(
        tools_dir.join("legacy.star"),
        r#"
            def legacy():
                return "ignored"
        "#,
    )
    .unwrap();

    let output = run_chidori(&["tools", "--dir", tools_dir.to_str().unwrap()], &dir);
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("web_search"));
    assert!(stdout.contains("Search the web."));
    assert!(!stdout.contains("legacy"));

    fs::remove_dir_all(dir).ok();
}

#[test]
fn cli_run_typescript_agent_invokes_typescript_tool() {
    let dir = temp_project("tool-run");
    let tools_dir = dir.join("tools");
    fs::create_dir_all(&tools_dir).unwrap();
    let agent = dir.join("agent.ts");
    fs::write(
        &agent,
        r#"
            export async function agent(input, chidori) {
                return await chidori.tool("echo", { value: input.value });
            }
        "#,
    )
    .unwrap();
    fs::write(
        tools_dir.join("echo.ts"),
        r#"
            export const tool = {
                name: "echo",
                description: "Echo a value.",
                parameters: {
                    type: "object",
                    properties: { value: { type: "number" } },
                    required: ["value"],
                },
            };
            export async function run(args, chidori) {
                await chidori.log("echo", args);
                return { echoed: args.value };
            }
        "#,
    )
    .unwrap();

    let output = run_chidori(
        &["run", agent.to_str().unwrap(), "--input", r#"{"value": 7}"#],
        &dir,
    );
    assert_success(&output);
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["echoed"], 7);

    fs::remove_dir_all(dir).ok();
}
