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
fn cli_run_typescript_agent_uses_definetool() {
    let dir = temp_project("run-definetool");
    let agent = dir.join("agent.ts");
    fs::write(
        &agent,
        r#"
            import { chidori, run, defineTool } from "chidori:agent";
            const webSearch = defineTool({
                name: "web_search",
                description: "Search the web for a short query.",
                parameters: {
                    type: "object",
                    properties: { query: { type: "string", description: "Search query" } },
                    required: ["query"],
                },
                run: async (args) => {
                    await chidori.log("web_search", { query: args.query });
                    return {
                        query: args.query,
                        results: [
                            { title: "Chidori tools", url: "https://example.test/chidori-tools" },
                        ],
                    };
                },
            });
            run(async (input) => {
                const searchResults = await webSearch.run(
                    { query: "latest developments " + input.topic },
                    chidori,
                );
                return { searchResults };
            });
        "#,
    )
    .unwrap();

    let output = run_chidori(
        &[
            "run",
            agent.to_str().unwrap(),
            "--trusted",
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
            "--trusted",
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

/// `chidori.workspace` must resolve out of the box (like the `docs` init
/// template), scoping to the agent's project directory even when
/// `CHIDORI_WORKSPACE_ROOT` is unset. Regression for the scaffolded `docs`
/// template failing with "requires CHIDORI_WORKSPACE_ROOT or a runtime
/// workspace root" on `chidori run`/`chat`.
#[test]
fn cli_workspace_defaults_to_project_dir_without_env() {
    let dir = temp_project("workspace-default-root");
    fs::create_dir_all(dir.join("docs")).unwrap();
    fs::write(dir.join("docs").join("readme.md"), "workspace-ok").unwrap();
    let agent = dir.join("agent.ts");
    fs::write(
        &agent,
        r#"
            export async function agent(input, chidori) {
                const entries = await chidori.workspace.list();
                const md = entries.map((e) => e.path).find((p) => p.endsWith("readme.md"));
                return { content: await chidori.workspace.read(md) };
            }
        "#,
    )
    .unwrap();

    // Note: no CHIDORI_WORKSPACE_ROOT — the CLI must fall back to the agent dir.
    let output = run_chidori(&["run", agent.to_str().unwrap(), "--input", r#"{}"#], &dir);
    assert_success(&output);
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["content"], "workspace-ok");

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
fn cli_serve_rejects_conflicting_trust_flags() {
    let dir = temp_project("serve-trust-flag-conflict");
    let agent = dir.join("agent.ts");
    fs::write(
        &agent,
        r#"
            export async function agent(input, chidori) {
                return { ok: true };
            }
        "#,
    )
    .unwrap();

    // --trusted and --untrusted contradict each other; clap must refuse the
    // combination before any server starts.
    let output = run_chidori(
        &["serve", agent.to_str().unwrap(), "--untrusted", "--trusted"],
        &dir,
    );
    assert!(
        !output.status.success(),
        "conflicting trust flags must not start a server"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--untrusted") && stderr.contains("--trusted"),
        "expected a flag-conflict error, got stderr:\n{stderr}"
    );

    fs::remove_dir_all(dir).ok();
}

#[test]
fn cli_stream_accepts_multiline_typed_agent_signature() {
    let dir = temp_project("stream-multiline-types");
    let agent = dir.join("agent.ts");
    fs::write(
        &agent,
        r#"
            import type { Chidori } from "chidori:agent";

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
fn cli_run_default_posture_fails_closed_on_gated_effects_without_terminal() {
    // With no policy configured, no --trusted, and no terminal to answer the
    // approval prompt, a gated effect (network fetch) must fail closed with an
    // actionable message instead of running fully trusted.
    let dir = temp_project("default-posture");
    let agent = dir.join("agent.ts");
    fs::write(
        &agent,
        r#"
            export async function agent(input, chidori) {
                return await fetch("https://example.test/gated");
            }
        "#,
    )
    .unwrap();

    let output = run_chidori(&["run", agent.to_str().unwrap()], &dir);
    assert_failure(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("requires approval"),
        "default posture should ask/deny, got:\n{stderr}"
    );
    assert!(
        stderr.contains("--trusted"),
        "denial should name the opt-out, got:\n{stderr}"
    );

    // The read-only workspace surface stays open without prompts.
    fs::remove_dir_all(dir).ok();
}

#[test]
fn cli_resume_refuses_changed_agent_source() {
    // `chidori resume` must verify the stored source fingerprints before
    // replaying the journal, exactly like the server resume routes: replay is
    // positional, so pairing cached results with changed code is a silent
    // correctness hazard.
    let dir = temp_project("resume-source-check");
    let agent = dir.join("agent.ts");
    fs::write(
        &agent,
        r#"
            export async function agent(input, chidori) {
                await chidori.log("original", {});
                return { version: 1 };
            }
        "#,
    )
    .unwrap();

    let output = run_chidori(&["run", agent.to_str().unwrap()], &dir);
    assert_success(&output);

    let runs_dir = dir.join(".chidori").join("runs");
    let run_id = fs::read_dir(&runs_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name()
        .into_string()
        .unwrap();

    // Unchanged source resumes fine.
    let output = run_chidori(&["resume", agent.to_str().unwrap(), &run_id], &dir);
    assert_success(&output);

    // Changed source is refused with a source-mismatch error.
    fs::write(
        &agent,
        r#"
            export async function agent(input, chidori) {
                await chidori.log("edited", {});
                return { version: 2 };
            }
        "#,
    )
    .unwrap();
    let output = run_chidori(&["resume", agent.to_str().unwrap(), &run_id], &dir);
    assert_failure(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("source") && stderr.contains("mismatch"),
        "expected a source mismatch refusal, got:\n{stderr}"
    );

    fs::remove_dir_all(dir).ok();
}

fn run_chidori_with_stdin(
    args: &[&str],
    cwd: &Path,
    envs: &[(&str, &str)],
    stdin_text: &str,
) -> Output {
    use std::io::Write as _;
    use std::process::Stdio;

    let mut command = Command::new(chidori_bin());
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in envs {
        command.env(key, value);
    }
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin_text.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

// `--stream` must run under the same posture as the plain `run` path: the
// agent's directory is the implicit workspace root, and the run journals under
// `.chidori/runs/<run_id>` — streaming changes progress reporting, not what
// the runtime can do or what survives.
#[test]
fn cli_stream_matches_plain_run_posture() {
    let dir = temp_project("stream-parity");
    let agent = dir.join("agent.ts");
    fs::write(
        &agent,
        r#"
            export async function agent(input, chidori) {
                await chidori.workspace.write("out.txt", "streamed", { language: "text" });
                return { ok: true };
            }
        "#,
    )
    .unwrap();

    let output = run_chidori(
        &[
            "run",
            agent.to_str().unwrap(),
            "--trusted",
            "--stream",
            "--input",
            r#"{}"#,
        ],
        &dir,
    );
    assert_success(&output);
    let events: Vec<serde_json::Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let done = events
        .iter()
        .find(|event| event["type"] == "done")
        .expect("stream ended without a done event");
    assert_eq!(done["status"], "completed");
    let run_id = done["run_id"].as_str().expect("done event carries run_id");

    // Workspace root defaulted to the agent's directory, no env var required.
    assert_eq!(fs::read_to_string(dir.join("out.txt")).unwrap(), "streamed");

    // The run journaled like a plain run: trace can read it back.
    assert!(dir.join(".chidori").join("runs").join(run_id).exists());
    assert_success(&run_chidori(&["trace", run_id], &dir));

    fs::remove_dir_all(dir).ok();
}

// A chat session is a durable run: the session id is announced, every turn
// journals under `.chidori/runs/<session_id>` with `input.json` holding the
// dialogue state, and `--resume` replays the transcript and continues the
// same session in place.
#[test]
fn cli_chat_session_persists_and_resumes() {
    let dir = temp_project("chat-persist");
    let envs = [("CHIDORI_TEST_LLM_RESPONSE", "canned reply")];

    let output = run_chidori_with_stdin(&["chat"], &dir, &envs, "hello there\nexit\n");
    assert_success(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let session_id = stderr
        .lines()
        .find(|line| line.starts_with("session ") && !line.starts_with("session saved"))
        .and_then(|line| line.strip_prefix("session "))
        .map(|rest| rest.split_whitespace().next().unwrap().to_string())
        .expect("chat announces its session id");
    assert!(
        stderr.contains("session saved"),
        "exit should point at --resume/trace, got:\n{stderr}"
    );
    // The reply is marked like the `you> ` prompt, so scrollback shows who
    // said what instead of one undifferentiated text column.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("assistant> "),
        "reply should carry an assistant> marker, got:\n{stdout}"
    );
    // Exit prints a usage/cost summary before the "session saved" line,
    // computed from the journaled records the same way `chidori stats` is.
    let summary = stderr
        .lines()
        .find(|line| line.starts_with("session usage:"))
        .expect("exit should print a session usage summary");
    assert!(
        summary.contains("1 prompt call(s)") && summary.contains("est. cost:"),
        "summary should count prompt calls and estimate cost, got:\n{summary}"
    );
    assert!(
        stderr.find("session usage:").unwrap() < stderr.find("session saved").unwrap(),
        "summary should precede the session-saved line, got:\n{stderr}"
    );

    let run_dir = dir.join(".chidori").join("runs").join(&session_id);
    let input: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(run_dir.join("input.json")).unwrap()).unwrap();
    assert_eq!(input["messages"], serde_json::json!(["hello there"]));

    // Resume: the restored transcript prints (prior turn replayed from the
    // journal), and the continued dialogue journals into the same run.
    let output = run_chidori_with_stdin(
        &["chat", "--resume", &session_id],
        &dir,
        &envs,
        "second message\nexit\n",
    );
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hello there") && stdout.contains("canned reply"),
        "resume should print the restored transcript, got:\n{stdout}"
    );
    let input: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(run_dir.join("input.json")).unwrap()).unwrap();
    assert_eq!(
        input["messages"],
        serde_json::json!(["hello there", "second message"])
    );

    // The session is an ordinary run to the rest of the toolchain.
    assert_success(&run_chidori(&["trace", &session_id], &dir));

    fs::remove_dir_all(dir).ok();
}
