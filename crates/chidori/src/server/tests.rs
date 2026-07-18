//! The server module's test suite, moved verbatim from the single-file
//! `server.rs` split (bodies unchanged; only the module-level imports below
//! were adjusted for the new module tree).

use super::*;
use crate::runtime::snapshot::{
    RuntimePolicy, SnapshotAbi, SourceFingerprint, HOST_PROMISE_TABLE_FILE,
};
use axum::body;
use axum::extract::{Path, State};
use std::sync::atomic::Ordering;

/// A failed session's `error` must carry stack frames in ORIGINAL
/// TypeScript coordinates for every frame, not just the throwing one —
/// the engine hands the server transpiled-bundle positions, and the
/// server (whose tokio handlers never set the CLI's thread-local display
/// root) remaps them against the agent's workspace root.
#[test]
fn agent_error_string_remaps_every_frame_to_original_source() {
    let dir = std::env::temp_dir().join(format!("chidori-srv-frames-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let agent_path = dir.join("agent.ts");
    // The interface block exists only in the original TypeScript, so every
    // transpiled line number below it disagrees with the original.
    let src = "interface Row {\n\
               \x20 id: string;\n\
               }\n\
               function inner(row: Row): never {\n\
               \x20 throw new Error(\"bad \" + row.id);\n\
               }\n\
               function outer(): never {\n\
               \x20 return inner({ id: \"x\" });\n\
               }\n\
               export async function agent() { return outer(); }\n";
    std::fs::write(&agent_path, src).unwrap();

    // Derive the frames' transpiled coordinates the same way the engine
    // stamps them: the emitted definition line of each function.
    let (js, _map) =
        crate::runtime::typescript::transpile::transpile_source_with_map(&agent_path, src).unwrap();
    let emitted_line = |name: &str| {
        js.lines()
            .position(|l| l.contains(&format!("function {name}")))
            .map(|i| i as u32 + 1)
            .unwrap()
    };
    let (inner_line, outer_line) = (emitted_line("inner"), emitted_line("outer"));
    // The interface strip is what makes this test meaningful: emitted
    // positions must disagree with the original definition lines (4, 7).
    assert_ne!(
        inner_line, 4,
        "transpile no longer shifts lines; rewrite this test"
    );

    let path_str = agent_path.to_string_lossy();
    let err = anyhow::anyhow!(
        "JavaScript exception: Error: bad x\n    at inner ({path_str}:{inner_line}:10)\n    at outer ({path_str}:{outer_line}:10)"
    );
    let remapped = agent_error_string(&agent_path, &err);
    assert!(
        remapped.contains(&format!("at inner ({path_str}:4:")),
        "throwing frame lands on the original definition line: {remapped}"
    );
    assert!(
        remapped.contains(&format!("at outer ({path_str}:7:")),
        "the frame ABOVE the throwing one lands on its original line too: {remapped}"
    );

    // Errors without frames pass through byte-identical.
    let plain = anyhow::anyhow!("policy: `tool:x` denied");
    assert_eq!(
        agent_error_string(&agent_path, &plain),
        "policy: `tool:x` denied"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn bearer_token_matches_single_key() {
    assert!(bearer_token_matches("Bearer sekrit", "sekrit"));
    assert!(!bearer_token_matches("Bearer wrong", "sekrit"));
    assert!(!bearer_token_matches("Bearer sekrit-longer", "sekrit"));
    assert!(!bearer_token_matches("Bearer sekri", "sekrit"));
    assert!(!bearer_token_matches("sekrit", "sekrit")); // missing scheme
    assert!(!bearer_token_matches("bearer sekrit", "sekrit")); // scheme is case-sensitive
}

#[test]
fn bearer_token_matches_rotating_key_list() {
    // During rotation both the new and the old key are accepted.
    assert!(bearer_token_matches("Bearer new-key", "new-key,old-key"));
    assert!(bearer_token_matches("Bearer old-key", "new-key,old-key"));
    assert!(!bearer_token_matches("Bearer other", "new-key,old-key"));
    // Whitespace around entries and empty entries are tolerated.
    assert!(bearer_token_matches("Bearer k2", " k1 , k2 ,"));
    // An empty list entry never matches an empty credential.
    assert!(!bearer_token_matches("Bearer ", ","));
}

#[test]
fn loopback_host_classification() {
    assert!(is_loopback_host("127.0.0.1"));
    assert!(is_loopback_host("127.0.0.2")); // whole 127.0.0.0/8 block
    assert!(is_loopback_host("::1"));
    assert!(is_loopback_host("localhost"));
    // Network-reachable binds — these require auth (or the explicit
    // CHIDORI_ALLOW_UNAUTHENTICATED opt-out) at startup.
    assert!(!is_loopback_host("0.0.0.0"));
    assert!(!is_loopback_host("::"));
    assert!(!is_loopback_host("10.0.0.5"));
    // Unparseable hostnames fail closed: treated as network-reachable.
    assert!(!is_loopback_host("my-host.internal"));
    assert!(!is_loopback_host(""));
}

fn test_run_base(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("{name}-{}", uuid::Uuid::new_v4()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn test_state(run_base: PathBuf, agent_path: PathBuf) -> AppState {
    AppState {
        providers: Arc::new(ProviderRegistry::new()),
        template_engine: Arc::new(TemplateEngine::new(".")),
        agent_path,
        has_default_agent: true,
        run_base,
        session_store: Arc::new(crate::storage::MemoryStore::new()),
        policy: PolicyConfig::from_env(),
        mcp: Arc::new(McpManager::new()),
        mcp_tools: Arc::new(Vec::new()),
        recipes: Arc::new(Vec::new()),
        run_semaphore: Arc::new(Semaphore::new(1)),
        acquire_timeout: std::time::Duration::from_millis(1),
        active_sessions: Arc::new(StdMutex::new(HashMap::new())),
        signal_inbox_locks: Arc::new(StdMutex::new(HashMap::new())),
        warm_runs: Arc::new(StdMutex::new(HashMap::new())),
        warm_evict: warm_evict_from_env(),
    }
}

async fn response_json(response: Response) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap();
    (status, value)
}

#[test]
fn stamp_attempt_adds_attempt_number_to_object_events() {
    let stamped = stamp_attempt(json!({"stream_id": "s1"}), Some(42));
    assert_eq!(stamped["attempt_number"], 42);
}

#[tokio::test]
async fn cancel_session_marks_active_session_cancelled() {
    let run_base = test_run_base("cancel_session_marks_active_session_cancelled");
    let agent_path = run_base.join("agent.ts");
    std::fs::write(
        &agent_path,
        "export default function agent() { return {}; }",
    )
    .unwrap();
    let state = test_state(run_base, agent_path);
    let (cancel_tx, mut cancel_rx) = tokio::sync::mpsc::unbounded_channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    state.active_sessions.lock().unwrap().insert(
        "session-1".to_string(),
        ActiveSession {
            cancelled: cancelled.clone(),
            cancel_tx,
            attempt_number: Some(7),
            signals: None,
        },
    );
    state
        .session_store
        .put(&StoredSession {
            id: "session-1".to_string(),
            run_id: None,
            status: SessionStatus::Running,
            input: json!({"ok": true}),
            output: None,
            call_log: Vec::new(),
            error: None,
            pending_seq: None,
            pending_prompt: None,
            pending_details: None,
            pending_signal_name: None,
            pending_signal_names: Vec::new(),
            pending_signal_deadline: None,
            pending_approval: None,
            approvals: Vec::new(),
            policy_profile: None,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

    let (status, body) = response_json(
        cancel_session(
            State(state.clone()),
            Path("session-1".to_string()),
            Some(Json(CancelSessionRequest {
                reason: Some("rewind".to_string()),
            })),
        )
        .await,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "cancelled");
    assert_eq!(body["active"], true);
    assert_eq!(body["attempt_number"], 7);
    assert_eq!(cancel_rx.recv().await.as_deref(), Some("rewind"));
    assert!(cancelled.load(Ordering::SeqCst));
    let stored = state.session_store.get("session-1").unwrap().unwrap();
    assert_eq!(stored.status, SessionStatus::Cancelled);
    assert_eq!(stored.error.as_deref(), Some("rewind"));
}

fn write_completed_host_promises(run_base: &FsPath, run_id: &str, records: Vec<HostPromiseRecord>) {
    write_host_promises(run_base, run_id, records);
}

fn write_host_promises(run_base: &FsPath, run_id: &str, records: Vec<HostPromiseRecord>) {
    let run_dir = run_base.join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(
        run_dir.join(HOST_PROMISE_TABLE_FILE),
        serde_json::to_vec_pretty(&records).unwrap(),
    )
    .unwrap();
}

fn write_snapshot_manifest_for_agent(run_base: &FsPath, run_id: &str, agent_path: &FsPath) {
    let source = std::fs::read_to_string(agent_path).unwrap();
    let manifest = crate::runtime::snapshot::SnapshotManifest::new(
        run_id,
        SnapshotAbi::current("chidori-quickjs"),
        RuntimePolicy::durable_default(run_id),
        SourceFingerprint::from_source(agent_path, &source),
        Vec::new(),
        None,
        0,
    );
    SnapshotStore::new(run_base.join(run_id))
        .save(&manifest, b"snapshot-bytes", &[])
        .unwrap();
}

#[test]
fn supported_agent_filenames_accept_ts_only() {
    assert!(is_supported_agent_filename("hello.ts"));

    assert!(!is_supported_agent_filename(""));
    assert!(!is_supported_agent_filename("legacy.star"));
    assert!(!is_supported_agent_filename("hello.js"));
    assert!(!is_supported_agent_filename("../hello.ts"));
    assert!(!is_supported_agent_filename("nested/hello.ts"));
    assert!(!is_supported_agent_filename("/tmp/hello.ts"));
    assert!(!is_supported_agent_filename("hello ts.ts"));
}

#[test]
fn resolve_agent_override_accepts_ts_peer_file() {
    let run_dir = std::env::temp_dir().join(format!(
        "chidori-server-agent-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&run_dir).unwrap();
    let default_agent = run_dir.join("default.ts");
    let alternate_agent = run_dir.join("alternate.ts");
    std::fs::write(&default_agent, "").unwrap();
    std::fs::write(&alternate_agent, "").unwrap();

    let resolved = resolve_agent_override(&default_agent, "alternate.ts").unwrap();
    assert_eq!(resolved, alternate_agent);

    let invalid = resolve_agent_override(&default_agent, "../alternate.ts").unwrap_err();
    assert_eq!(invalid.0, StatusCode::BAD_REQUEST);

    let missing = resolve_agent_override(&default_agent, "missing.ts").unwrap_err();
    assert_eq!(missing.0, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(run_dir);
}

#[test]
fn checkpoint_snapshot_manifest_is_loaded_by_run_id() {
    let temp_dir = std::env::temp_dir().join(format!(
        "chidori-server-snapshot-test-{}",
        uuid::Uuid::new_v4()
    ));
    let run_base = temp_dir.join(".chidori").join("runs");
    let run_id = "run-1";
    let store = crate::runtime::snapshot::SnapshotStore::new(run_base.join(run_id));
    let manifest = crate::runtime::snapshot::SnapshotManifest::new(
        run_id,
        crate::runtime::snapshot::SnapshotAbi::current("chidori-quickjs"),
        crate::runtime::snapshot::RuntimePolicy::durable_default(run_id),
        crate::runtime::snapshot::SourceFingerprint::from_source(
            "agent.ts",
            "export async function agent() {}",
        ),
        Vec::new(),
        None,
        0,
    );
    store.save(&manifest, b"snapshot-bytes", &[]).unwrap();

    let state = test_state(run_base, temp_dir.join("agent.ts"));
    let session = StoredSession {
        id: "session-1".to_string(),
        run_id: Some(run_id.to_string()),
        status: SessionStatus::Completed,
        input: Value::Null,
        output: None,
        call_log: Vec::new(),
        error: None,
        pending_seq: None,
        pending_prompt: None,
        pending_details: None,
        pending_signal_name: None,
        pending_signal_names: Vec::new(),
        pending_signal_deadline: None,
        pending_approval: None,
        approvals: Vec::new(),
        policy_profile: None,
        created_at: chrono::Utc::now(),
    };

    let loaded = snapshot_manifest_for_session(&state, &session).unwrap();
    assert_eq!(loaded["run_id"], run_id);
    assert_eq!(loaded["snapshot_file"], "runtime.snapshot");

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[tokio::test]
async fn replay_session_uses_completed_prompt_host_promise_without_provider() {
    let temp_dir = std::env::temp_dir().join(format!(
        "chidori-server-replay-prompt-host-promise-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let agent_path = temp_dir.join("agent.ts");
    std::fs::write(
        &agent_path,
        r#"
            export async function agent(input, chidori) {
                const text = await chidori.prompt("hello", { type: "progress" });
                return { text };
            }
        "#,
    )
    .unwrap();
    let run_base = temp_dir.join(".chidori").join("runs");
    let run_id = "old-run";
    write_completed_host_promises(
        &run_base,
        run_id,
        vec![HostPromiseRecord {
            operation: PendingHostOperation::new(
                crate::runtime::snapshot::HostOperationId(1),
                1,
                PendingHostOperationKind::Prompt,
                json!({
                    "text": "hello",
                    "model": "claude-sonnet-4-6",
                    "type": "progress",
                }),
            ),
            state: HostPromiseState::Resolved {
                value: json!("cached prompt"),
                completed_at: chrono::Utc::now(),
            },
        }],
    );
    let state = test_state(run_base, agent_path);
    let session = StoredSession {
        id: "session-1".to_string(),
        run_id: Some(run_id.to_string()),
        status: SessionStatus::Completed,
        input: json!({}),
        output: None,
        call_log: Vec::new(),
        error: None,
        pending_seq: None,
        pending_prompt: None,
        pending_details: None,
        pending_signal_name: None,
        pending_signal_names: Vec::new(),
        pending_signal_deadline: None,
        pending_approval: None,
        approvals: Vec::new(),
        policy_profile: None,
        created_at: chrono::Utc::now(),
    };
    state.session_store.put(&session).unwrap();

    let (status, body) =
        response_json(replay_session(State(state), Path("session-1".to_string())).await).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["output"], json!({ "text": "cached prompt" }));
    assert_eq!(body["status"], json!("completed"));

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[tokio::test]
async fn replay_session_uses_completed_tool_host_promise_without_tool_registry() {
    let temp_dir = std::env::temp_dir().join(format!(
        "chidori-server-replay-tool-host-promise-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let agent_path = temp_dir.join("agent.ts");
    std::fs::write(
        &agent_path,
        r#"
            export async function agent(input, chidori) {
                return await chidori.tool("missing", { value: input.value });
            }
        "#,
    )
    .unwrap();
    let run_base = temp_dir.join(".chidori").join("runs");
    let run_id = "old-run";
    write_completed_host_promises(
        &run_base,
        run_id,
        vec![HostPromiseRecord {
            operation: PendingHostOperation::new(
                crate::runtime::snapshot::HostOperationId(1),
                1,
                PendingHostOperationKind::Tool,
                json!({
                    "name": "missing",
                    "kwargs": { "value": 41 },
                }),
            ),
            state: HostPromiseState::Resolved {
                value: json!({ "value": 42 }),
                completed_at: chrono::Utc::now(),
            },
        }],
    );
    let state = test_state(run_base, agent_path);
    let session = StoredSession {
        id: "session-1".to_string(),
        run_id: Some(run_id.to_string()),
        status: SessionStatus::Completed,
        input: json!({ "value": 41 }),
        output: None,
        call_log: Vec::new(),
        error: None,
        pending_seq: None,
        pending_prompt: None,
        pending_details: None,
        pending_signal_name: None,
        pending_signal_names: Vec::new(),
        pending_signal_deadline: None,
        pending_approval: None,
        approvals: Vec::new(),
        policy_profile: None,
        created_at: chrono::Utc::now(),
    };
    state.session_store.put(&session).unwrap();

    let (status, body) =
        response_json(replay_session(State(state), Path("session-1".to_string())).await).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["output"], json!({ "value": 42 }));

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[tokio::test]
async fn replay_session_uses_completed_call_agent_host_promise_without_child_file() {
    let temp_dir = std::env::temp_dir().join(format!(
        "chidori-server-replay-call-agent-host-promise-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let agent_path = temp_dir.join("agent.ts");
    let child_path = temp_dir.join("missing-child.ts");
    let child_path_string = child_path.display().to_string();
    let child_path_json = serde_json::to_string(&child_path_string).unwrap();
    std::fs::write(
        &agent_path,
        format!(
            r#"
            export async function agent(input, chidori) {{
                return await chidori.callAgent({child_path_json}, {{ value: input.value }});
            }}
            "#
        ),
    )
    .unwrap();
    let run_base = temp_dir.join(".chidori").join("runs");
    let run_id = "old-run";
    write_completed_host_promises(
        &run_base,
        run_id,
        vec![HostPromiseRecord {
            operation: PendingHostOperation::new(
                crate::runtime::snapshot::HostOperationId(1),
                1,
                PendingHostOperationKind::CallAgent,
                json!({
                    "path": child_path_string,
                    "input": { "value": 41 },
                }),
            ),
            state: HostPromiseState::Resolved {
                value: json!({ "value": 42 }),
                completed_at: chrono::Utc::now(),
            },
        }],
    );
    let state = test_state(run_base, agent_path);
    let session = StoredSession {
        id: "session-1".to_string(),
        run_id: Some(run_id.to_string()),
        status: SessionStatus::Completed,
        input: json!({ "value": 41 }),
        output: None,
        call_log: Vec::new(),
        error: None,
        pending_seq: None,
        pending_prompt: None,
        pending_details: None,
        pending_signal_name: None,
        pending_signal_names: Vec::new(),
        pending_signal_deadline: None,
        pending_approval: None,
        approvals: Vec::new(),
        policy_profile: None,
        created_at: chrono::Utc::now(),
    };
    state.session_store.put(&session).unwrap();

    let (status, body) =
        response_json(replay_session(State(state), Path("session-1".to_string())).await).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["output"], json!({ "value": 42 }));

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[tokio::test]
async fn resume_session_uses_completed_prompt_host_promise_without_provider() {
    let temp_dir = std::env::temp_dir().join(format!(
        "chidori-server-resume-prompt-host-promise-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let agent_path = temp_dir.join("agent.ts");
    std::fs::write(
        &agent_path,
        r#"
            export async function agent(input, chidori) {
                const text = await chidori.prompt("hello", { type: "progress" });
                const approved = await chidori.input("continue?");
                return { text, approved };
            }
        "#,
    )
    .unwrap();
    let run_base = temp_dir.join(".chidori").join("runs");
    let run_id = "old-run";
    write_snapshot_manifest_for_agent(&run_base, run_id, &agent_path);
    let pending_input = PendingHostOperation::new(
        crate::runtime::snapshot::HostOperationId(2),
        2,
        PendingHostOperationKind::Input,
        json!({ "prompt": "continue?" }),
    );
    std::fs::write(
        run_base.join(run_id).join(PENDING_HOST_OPERATION_FILE),
        serde_json::to_vec_pretty(&pending_input).unwrap(),
    )
    .unwrap();
    write_host_promises(
        &run_base,
        run_id,
        vec![
            HostPromiseRecord {
                operation: PendingHostOperation::new(
                    crate::runtime::snapshot::HostOperationId(1),
                    1,
                    PendingHostOperationKind::Prompt,
                    json!({
                        "text": "hello",
                        "model": "claude-sonnet-4-6",
                        "type": "progress",
                    }),
                ),
                state: HostPromiseState::Resolved {
                    value: json!("cached prompt"),
                    completed_at: chrono::Utc::now(),
                },
            },
            HostPromiseRecord {
                operation: pending_input,
                state: HostPromiseState::Pending,
            },
        ],
    );
    let state = test_state(run_base, agent_path);
    let session = StoredSession {
        id: "session-1".to_string(),
        run_id: Some(run_id.to_string()),
        status: SessionStatus::Paused,
        input: json!({}),
        output: None,
        call_log: Vec::new(),
        error: None,
        pending_seq: Some(2),
        pending_prompt: Some("continue?".to_string()),
        pending_details: None,
        pending_signal_name: None,
        pending_signal_names: Vec::new(),
        pending_signal_deadline: None,
        pending_approval: None,
        approvals: Vec::new(),
        policy_profile: None,
        created_at: chrono::Utc::now(),
    };
    state.session_store.put(&session).unwrap();

    let (status, body) = response_json(
        resume_session(
            State(state.clone()),
            Path("session-1".to_string()),
            Json(ResumeRequest {
                response: "yes".to_string(),
                allow_source_change: false,
            }),
        )
        .await,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], json!("completed"));
    assert_eq!(
        body["output"],
        json!({ "text": "cached prompt", "approved": "yes" })
    );

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[tokio::test]
async fn resume_session_uses_completed_tool_host_promise_without_tool_registry() {
    let temp_dir = std::env::temp_dir().join(format!(
        "chidori-server-resume-tool-host-promise-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let agent_path = temp_dir.join("agent.ts");
    std::fs::write(
        &agent_path,
        r#"
            export async function agent(input, chidori) {
                const value = await chidori.tool("missing", { value: input.value });
                const approved = await chidori.input("continue?");
                return { value, approved };
            }
        "#,
    )
    .unwrap();
    let run_base = temp_dir.join(".chidori").join("runs");
    let run_id = "old-run";
    write_snapshot_manifest_for_agent(&run_base, run_id, &agent_path);
    let pending_input = PendingHostOperation::new(
        crate::runtime::snapshot::HostOperationId(2),
        2,
        PendingHostOperationKind::Input,
        json!({ "prompt": "continue?" }),
    );
    std::fs::write(
        run_base.join(run_id).join(PENDING_HOST_OPERATION_FILE),
        serde_json::to_vec_pretty(&pending_input).unwrap(),
    )
    .unwrap();
    write_host_promises(
        &run_base,
        run_id,
        vec![
            HostPromiseRecord {
                operation: PendingHostOperation::new(
                    crate::runtime::snapshot::HostOperationId(1),
                    1,
                    PendingHostOperationKind::Tool,
                    json!({
                        "name": "missing",
                        "kwargs": { "value": 41 },
                    }),
                ),
                state: HostPromiseState::Resolved {
                    value: json!({ "value": 42 }),
                    completed_at: chrono::Utc::now(),
                },
            },
            HostPromiseRecord {
                operation: pending_input,
                state: HostPromiseState::Pending,
            },
        ],
    );
    let state = test_state(run_base, agent_path);
    let session = StoredSession {
        id: "session-1".to_string(),
        run_id: Some(run_id.to_string()),
        status: SessionStatus::Paused,
        input: json!({ "value": 41 }),
        output: None,
        call_log: Vec::new(),
        error: None,
        pending_seq: Some(2),
        pending_prompt: Some("continue?".to_string()),
        pending_details: None,
        pending_signal_name: None,
        pending_signal_names: Vec::new(),
        pending_signal_deadline: None,
        pending_approval: None,
        approvals: Vec::new(),
        policy_profile: None,
        created_at: chrono::Utc::now(),
    };
    state.session_store.put(&session).unwrap();

    let (status, body) = response_json(
        resume_session(
            State(state.clone()),
            Path("session-1".to_string()),
            Json(ResumeRequest {
                response: "yes".to_string(),
                allow_source_change: false,
            }),
        )
        .await,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], json!("completed"));
    assert_eq!(
        body["output"],
        json!({ "value": { "value": 42 }, "approved": "yes" })
    );

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[tokio::test]
async fn resume_session_uses_completed_call_agent_host_promise_without_child_file() {
    let temp_dir = std::env::temp_dir().join(format!(
        "chidori-server-resume-call-agent-host-promise-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let agent_path = temp_dir.join("agent.ts");
    let child_path = temp_dir.join("missing-child.ts");
    let child_path_string = child_path.display().to_string();
    let child_path_json = serde_json::to_string(&child_path_string).unwrap();
    std::fs::write(
        &agent_path,
        format!(
            r#"
            export async function agent(input, chidori) {{
                const value = await chidori.callAgent({child_path_json}, {{ value: input.value }});
                const approved = await chidori.input("continue?");
                return {{ value, approved }};
            }}
            "#
        ),
    )
    .unwrap();
    let run_base = temp_dir.join(".chidori").join("runs");
    let run_id = "old-run";
    write_snapshot_manifest_for_agent(&run_base, run_id, &agent_path);
    let pending_input = PendingHostOperation::new(
        crate::runtime::snapshot::HostOperationId(2),
        2,
        PendingHostOperationKind::Input,
        json!({ "prompt": "continue?" }),
    );
    std::fs::write(
        run_base.join(run_id).join(PENDING_HOST_OPERATION_FILE),
        serde_json::to_vec_pretty(&pending_input).unwrap(),
    )
    .unwrap();
    write_host_promises(
        &run_base,
        run_id,
        vec![
            HostPromiseRecord {
                operation: PendingHostOperation::new(
                    crate::runtime::snapshot::HostOperationId(1),
                    1,
                    PendingHostOperationKind::CallAgent,
                    json!({
                        "path": child_path_string,
                        "input": { "value": 41 },
                    }),
                ),
                state: HostPromiseState::Resolved {
                    value: json!({ "value": 42 }),
                    completed_at: chrono::Utc::now(),
                },
            },
            HostPromiseRecord {
                operation: pending_input,
                state: HostPromiseState::Pending,
            },
        ],
    );
    let state = test_state(run_base, agent_path);
    let session = StoredSession {
        id: "session-1".to_string(),
        run_id: Some(run_id.to_string()),
        status: SessionStatus::Paused,
        input: json!({ "value": 41 }),
        output: None,
        call_log: Vec::new(),
        error: None,
        pending_seq: Some(2),
        pending_prompt: Some("continue?".to_string()),
        pending_details: None,
        pending_signal_name: None,
        pending_signal_names: Vec::new(),
        pending_signal_deadline: None,
        pending_approval: None,
        approvals: Vec::new(),
        policy_profile: None,
        created_at: chrono::Utc::now(),
    };
    state.session_store.put(&session).unwrap();

    let (status, body) = response_json(
        resume_session(
            State(state.clone()),
            Path("session-1".to_string()),
            Json(ResumeRequest {
                response: "yes".to_string(),
                allow_source_change: false,
            }),
        )
        .await,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], json!("completed"));
    assert_eq!(
        body["output"],
        json!({ "value": { "value": 42 }, "approved": "yes" })
    );

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn resume_snapshot_validation_rejects_source_mismatch() {
    let temp_dir = std::env::temp_dir().join(format!(
        "chidori-server-snapshot-source-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let run_base = temp_dir.join(".chidori").join("runs");
    let run_id = "run-1";
    let agent_path = temp_dir.join("agent.ts");
    std::fs::write(&agent_path, "export async function agent() { return 1; }").unwrap();

    let store = SnapshotStore::new(run_base.join(run_id));
    let manifest = crate::runtime::snapshot::SnapshotManifest::new(
        run_id,
        SnapshotAbi::current("chidori-quickjs"),
        RuntimePolicy::durable_default(run_id),
        SourceFingerprint::from_source(&agent_path, "export async function agent() { return 1; }"),
        Vec::new(),
        None,
        0,
    );
    store.save(&manifest, b"snapshot-bytes", &[]).unwrap();
    std::fs::write(&agent_path, "export async function agent() { return 2; }").unwrap();

    let err = validate_snapshot_manifest_for_resume(&run_base, Some(run_id), &agent_path, false)
        .unwrap_err();
    assert!(format!("{err:#}").contains("runtime snapshot source mismatch"));
    // The refusal advertises the edit-and-resume opt-in…
    assert!(format!("{err:#}").contains("allow-source-change"));
    // …and opting in lets the same resume proceed.
    validate_snapshot_manifest_for_resume(&run_base, Some(run_id), &agent_path, true).unwrap();

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn resume_snapshot_validation_rejects_module_graph_mismatch() {
    let temp_dir = std::env::temp_dir().join(format!(
        "chidori-server-snapshot-module-graph-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let run_base = temp_dir.join(".chidori").join("runs");
    let run_id = "run-1";
    let agent_path = temp_dir.join("agent.ts");
    let module_path = temp_dir.join("lib.ts");
    let wrong_module_path = temp_dir.join("other.ts");
    let source = r#"
        import { value } from "./lib.ts";
        export async function agent() { return value; }
    "#;
    let module_source = "export const value = 1;";
    std::fs::write(&agent_path, source).unwrap();
    std::fs::write(&module_path, module_source).unwrap();
    std::fs::write(&wrong_module_path, "export const value = 2;").unwrap();

    let store = SnapshotStore::new(run_base.join(run_id));
    let manifest = crate::runtime::snapshot::SnapshotManifest::new(
        run_id,
        SnapshotAbi::current("chidori-quickjs"),
        RuntimePolicy::durable_default(run_id),
        SourceFingerprint::from_source(&agent_path, source),
        vec![SourceFingerprint::from_source(&module_path, module_source)],
        None,
        0,
    )
    .with_module_graph(vec![
        crate::runtime::snapshot::SnapshotModuleGraphEntry {
            path: agent_path.clone(),
            imports: vec![crate::runtime::snapshot::SnapshotModuleImport {
                specifier: "./lib.ts".to_string(),
                resolved_path: Some(wrong_module_path),
            }],
        },
        crate::runtime::snapshot::SnapshotModuleGraphEntry {
            path: module_path,
            imports: Vec::new(),
        },
    ]);
    store.save(&manifest, b"snapshot-bytes", &[]).unwrap();

    let err = validate_snapshot_manifest_for_resume(&run_base, Some(run_id), &agent_path, false)
        .unwrap_err();
    assert!(format!("{err:#}").contains("runtime snapshot module graph mismatch"));

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn resume_snapshot_validation_rejects_abi_mismatch() {
    let temp_dir = std::env::temp_dir().join(format!(
        "chidori-server-snapshot-abi-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let run_base = temp_dir.join(".chidori").join("runs");
    let run_id = "run-1";
    let agent_path = temp_dir.join("agent.ts");
    let source = "export async function agent() { return 1; }";
    std::fs::write(&agent_path, source).unwrap();

    let store = SnapshotStore::new(run_base.join(run_id));
    let manifest = crate::runtime::snapshot::SnapshotManifest::new(
        run_id,
        SnapshotAbi::current("different-fork"),
        RuntimePolicy::durable_default(run_id),
        SourceFingerprint::from_source(&agent_path, source),
        Vec::new(),
        None,
        0,
    );
    store.save(&manifest, b"snapshot-bytes", &[]).unwrap();

    let err = validate_snapshot_manifest_for_resume(&run_base, Some(run_id), &agent_path, false)
        .unwrap_err();
    assert!(err.to_string().contains("runtime snapshot ABI mismatch"));
    // ABI drift is environment skew, not an edit: the edit-and-resume
    // opt-in must NOT bypass it.
    let err = validate_snapshot_manifest_for_resume(&run_base, Some(run_id), &agent_path, true)
        .unwrap_err();
    assert!(err.to_string().contains("runtime snapshot ABI mismatch"));

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn resume_resolves_persisted_pending_input_host_operation() {
    let temp_dir = std::env::temp_dir().join(format!(
        "chidori-server-resume-host-promise-test-{}",
        uuid::Uuid::new_v4()
    ));
    let run_base = temp_dir.join(".chidori").join("runs");
    let run_id = "run-1";
    let run_dir = run_base.join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();

    let pending = PendingHostOperation::new(
        crate::runtime::snapshot::HostOperationId(7),
        3,
        PendingHostOperationKind::Input,
        json!({ "prompt": "Approve?" }),
    );
    let records = vec![HostPromiseRecord {
        operation: pending.clone(),
        state: HostPromiseState::Pending,
    }];
    std::fs::write(
        run_dir.join(PENDING_HOST_OPERATION_FILE),
        serde_json::to_vec_pretty(&pending).unwrap(),
    )
    .unwrap();
    std::fs::write(
        run_dir.join(HOST_PROMISE_TABLE_FILE),
        serde_json::to_vec_pretty(&records).unwrap(),
    )
    .unwrap();

    resolve_persisted_pending_host_operation(
        &run_base,
        Some(run_id),
        3,
        PendingHostOperationKind::Input,
        json!("yes"),
    )
    .unwrap();

    assert!(!run_dir.join(PENDING_HOST_OPERATION_FILE).exists());
    let records: Vec<HostPromiseRecord> =
        serde_json::from_slice(&std::fs::read(run_dir.join(HOST_PROMISE_TABLE_FILE)).unwrap())
            .unwrap();
    assert_eq!(records.len(), 1);
    match &records[0].state {
        HostPromiseState::Resolved { value, .. } => assert_eq!(value, &json!("yes")),
        other => panic!("expected resolved host promise, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn resume_rejects_persisted_pending_host_operation_missing_from_table() {
    let temp_dir = std::env::temp_dir().join(format!(
        "chidori-server-resume-missing-host-promise-test-{}",
        uuid::Uuid::new_v4()
    ));
    let run_base = temp_dir.join(".chidori").join("runs");
    let run_id = "run-1";
    let run_dir = run_base.join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();

    let pending = PendingHostOperation::new(
        crate::runtime::snapshot::HostOperationId(7),
        3,
        PendingHostOperationKind::Input,
        json!({ "prompt": "Approve?" }),
    );
    std::fs::write(
        run_dir.join(PENDING_HOST_OPERATION_FILE),
        serde_json::to_vec_pretty(&pending).unwrap(),
    )
    .unwrap();
    std::fs::write(
        run_dir.join(HOST_PROMISE_TABLE_FILE),
        serde_json::to_vec_pretty(&Vec::<HostPromiseRecord>::new()).unwrap(),
    )
    .unwrap();

    let err = resolve_persisted_pending_host_operation(
        &run_base,
        Some(run_id),
        3,
        PendingHostOperationKind::Input,
        json!("yes"),
    )
    .unwrap_err();

    assert!(err
        .to_string()
        .contains("missing from persisted host promise table"));
    assert!(run_dir.join(PENDING_HOST_OPERATION_FILE).exists());
    let records: Vec<HostPromiseRecord> =
        serde_json::from_slice(&std::fs::read(run_dir.join(HOST_PROMISE_TABLE_FILE)).unwrap())
            .unwrap();
    assert!(records.is_empty());

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn resume_rejects_already_completed_persisted_pending_host_operation() {
    let temp_dir = std::env::temp_dir().join(format!(
        "chidori-server-resume-completed-host-promise-test-{}",
        uuid::Uuid::new_v4()
    ));
    let run_base = temp_dir.join(".chidori").join("runs");
    let run_id = "run-1";
    let run_dir = run_base.join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();

    let pending = PendingHostOperation::new(
        crate::runtime::snapshot::HostOperationId(7),
        3,
        PendingHostOperationKind::Input,
        json!({ "prompt": "Approve?" }),
    );
    let records = vec![HostPromiseRecord {
        operation: pending.clone(),
        state: HostPromiseState::Resolved {
            value: json!("old"),
            completed_at: chrono::Utc::now(),
        },
    }];
    std::fs::write(
        run_dir.join(PENDING_HOST_OPERATION_FILE),
        serde_json::to_vec_pretty(&pending).unwrap(),
    )
    .unwrap();
    std::fs::write(
        run_dir.join(HOST_PROMISE_TABLE_FILE),
        serde_json::to_vec_pretty(&records).unwrap(),
    )
    .unwrap();

    let err = resolve_persisted_pending_host_operation(
        &run_base,
        Some(run_id),
        3,
        PendingHostOperationKind::Input,
        json!("yes"),
    )
    .unwrap_err();

    assert!(err
        .to_string()
        .contains("already completed in persisted host promise table"));
    assert!(run_dir.join(PENDING_HOST_OPERATION_FILE).exists());
    let records: Vec<HostPromiseRecord> =
        serde_json::from_slice(&std::fs::read(run_dir.join(HOST_PROMISE_TABLE_FILE)).unwrap())
            .unwrap();
    match &records[0].state {
        HostPromiseState::Resolved { value, .. } => assert_eq!(value, &json!("old")),
        other => panic!("expected original resolved host promise, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn approval_allow_resolves_persisted_pending_host_operation() {
    let temp_dir = std::env::temp_dir().join(format!(
        "chidori-server-approval-allow-host-promise-test-{}",
        uuid::Uuid::new_v4()
    ));
    let run_base = temp_dir.join(".chidori").join("runs");
    let run_id = "run-1";
    let run_dir = run_base.join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();

    let pending = PendingHostOperation::new(
        crate::runtime::snapshot::HostOperationId(9),
        5,
        PendingHostOperationKind::Tool,
        json!({ "name": "deploy", "kwargs": {} }),
    );
    let records = vec![HostPromiseRecord {
        operation: pending.clone(),
        state: HostPromiseState::Pending,
    }];
    std::fs::write(
        run_dir.join(PENDING_HOST_OPERATION_FILE),
        serde_json::to_vec_pretty(&pending).unwrap(),
    )
    .unwrap();
    std::fs::write(
        run_dir.join(HOST_PROMISE_TABLE_FILE),
        serde_json::to_vec_pretty(&records).unwrap(),
    )
    .unwrap();

    complete_persisted_pending_host_operation(
        &run_base,
        Some(run_id),
        None,
        HostPromiseCompletion::Resolved(json!({
            "approved": true,
            "target": "tool:deploy",
        })),
    )
    .unwrap();

    assert!(!run_dir.join(PENDING_HOST_OPERATION_FILE).exists());
    let records: Vec<HostPromiseRecord> =
        serde_json::from_slice(&std::fs::read(run_dir.join(HOST_PROMISE_TABLE_FILE)).unwrap())
            .unwrap();
    assert_eq!(records.len(), 1);
    match &records[0].state {
        HostPromiseState::Resolved { value, .. } => {
            assert_eq!(value["approved"], json!(true));
            assert_eq!(value["target"], json!("tool:deploy"));
        }
        other => panic!("expected resolved host promise, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn approval_deny_rejects_persisted_pending_host_operation() {
    let temp_dir = std::env::temp_dir().join(format!(
        "chidori-server-approval-deny-host-promise-test-{}",
        uuid::Uuid::new_v4()
    ));
    let run_base = temp_dir.join(".chidori").join("runs");
    let run_id = "run-1";
    let run_dir = run_base.join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();

    let pending = PendingHostOperation::new(
        crate::runtime::snapshot::HostOperationId(10),
        6,
        PendingHostOperationKind::Http,
        json!({ "url": "https://example.invalid" }),
    );
    let records = vec![HostPromiseRecord {
        operation: pending.clone(),
        state: HostPromiseState::Pending,
    }];
    std::fs::write(
        run_dir.join(PENDING_HOST_OPERATION_FILE),
        serde_json::to_vec_pretty(&pending).unwrap(),
    )
    .unwrap();
    std::fs::write(
        run_dir.join(HOST_PROMISE_TABLE_FILE),
        serde_json::to_vec_pretty(&records).unwrap(),
    )
    .unwrap();

    complete_persisted_pending_host_operation(
        &run_base,
        Some(run_id),
        None,
        HostPromiseCompletion::Rejected(
            "policy: `http:https://example.invalid` denied by operator".to_string(),
        ),
    )
    .unwrap();

    assert!(!run_dir.join(PENDING_HOST_OPERATION_FILE).exists());
    let records: Vec<HostPromiseRecord> =
        serde_json::from_slice(&std::fs::read(run_dir.join(HOST_PROMISE_TABLE_FILE)).unwrap())
            .unwrap();
    assert_eq!(records.len(), 1);
    match &records[0].state {
        HostPromiseState::Rejected { error, .. } => {
            assert_eq!(
                error,
                "policy: `http:https://example.invalid` denied by operator"
            );
        }
        other => panic!("expected rejected host promise, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(temp_dir);
}

fn policy_test_project(name: &str, agent_source: &str) -> (PathBuf, AppState) {
    let temp_dir = std::env::temp_dir().join(format!("{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let agent_path = temp_dir.join("agent.ts");
    std::fs::write(&agent_path, agent_source).unwrap();
    let run_base = temp_dir.join(".chidori").join("runs");
    let state = test_state(run_base, agent_path);
    (temp_dir, state)
}

/// True while `id`'s engine thread is warm-parked at an input pause.
fn warm_parked(state: &AppState, id: &str) -> bool {
    state
        .warm_runs
        .lock()
        .unwrap()
        .get(id)
        .map(|warm| warm.resolution.lock().unwrap().is_some())
        .unwrap_or(false)
}

const TWO_INPUT_AGENT: &str = r#"
    export async function agent(input, chidori) {
        const a = await chidori.input("first?");
        const b = await chidori.input("second?");
        return { a, b };
    }
"#;

fn warm_create_request(session_id: &str) -> CreateSessionRequest {
    CreateSessionRequest {
        input: json!({}),
        session_id: Some(session_id.to_string()),
        attempt_number: None,
        replay_from: None,
        agent: None,
        policy_profile: None,
    }
}

async fn resume_with(state: &AppState, id: &str, response: &str) -> (StatusCode, Value) {
    response_json(
        resume_session(
            State(state.clone()),
            Path(id.to_string()),
            Json(ResumeRequest {
                response: response.to_string(),
                allow_source_change: false,
            }),
        )
        .await,
    )
    .await
}

/// Records that matter for replay parity: (seq, function, args, result).
fn journal_shape(run_base: &FsPath, run_id: &str) -> Vec<(u64, String, Value, Value)> {
    use crate::runtime::store::RunStore as _;
    crate::runtime::store::FsRunStore::new(run_base.join(run_id))
        .load_call_log()
        .unwrap()
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.seq, r.function, r.args, r.result))
        .collect()
}

/// End-to-end warm resume: the run parks its live VM at each input pause,
/// /resume continues it in place, and the terminal outcome retires the
/// warm entry.
#[tokio::test]
async fn warm_resume_continues_parked_run_in_place() {
    let temp_dir = std::env::temp_dir().join(format!(
        "chidori-server-warm-resume-{}",
        uuid::Uuid::new_v4()
    ));
    let agent_path = write_agent(&temp_dir, TWO_INPUT_AGENT);
    let run_base = temp_dir.join(".chidori").join("runs");
    std::fs::create_dir_all(&run_base).unwrap();
    let state = test_state(run_base.clone(), agent_path);

    let (status, body) = response_json(
        create_session(State(state.clone()), Json(warm_create_request("warm-1"))).await,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["status"], json!("paused"));
    assert_eq!(body["pending_prompt"], json!("first?"));
    assert!(
        warm_parked(&state, "warm-1"),
        "engine thread must be warm-parked at the first pause"
    );

    let (status, body) = resume_with(&state, "warm-1", "A").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], json!("paused"));
    assert_eq!(body["pending_prompt"], json!("second?"));
    assert!(warm_parked(&state, "warm-1"), "second pause must park too");

    let (status, body) = resume_with(&state, "warm-1", "B").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], json!("completed"));
    assert_eq!(body["output"], json!({ "a": "A", "b": "B" }));
    assert!(
        state.warm_runs.lock().unwrap().get("warm-1").is_none(),
        "terminal outcome must retire the warm entry"
    );

    // Durable parity: the journal carries the same input records the
    // replay path would have injected.
    let run_id = state
        .session_store
        .get("warm-1")
        .unwrap()
        .unwrap()
        .run_id
        .unwrap();
    let journal = journal_shape(&run_base, &run_id);
    assert_eq!(journal.len(), 2);
    assert_eq!(
        journal[0],
        (
            1,
            "input".to_string(),
            json!({ "prompt": "first?" }),
            json!("A")
        )
    );
    assert_eq!(
        journal[1],
        (
            2,
            "input".to_string(),
            json!({ "prompt": "second?" }),
            json!("B")
        )
    );

    let _ = std::fs::remove_dir_all(temp_dir);
}

/// The warm and replay paths must be interchangeable: force the fallback
/// (retire the entry mid-pause, as a crash/restart would) and assert the
/// journal comes out identical to the warm run's.
#[tokio::test]
async fn warm_fallback_replay_produces_identical_journal() {
    let temp_dir = std::env::temp_dir().join(format!(
        "chidori-server-warm-fallback-{}",
        uuid::Uuid::new_v4()
    ));
    let agent_path = write_agent(&temp_dir, TWO_INPUT_AGENT);
    let run_base = temp_dir.join(".chidori").join("runs");
    std::fs::create_dir_all(&run_base).unwrap();
    let state = test_state(run_base.clone(), agent_path);

    // Warm reference run.
    let (_, body) = response_json(
        create_session(State(state.clone()), Json(warm_create_request("warm-ref"))).await,
    )
    .await;
    assert_eq!(body["status"], json!("paused"));
    resume_with(&state, "warm-ref", "A").await;
    let (_, body) = resume_with(&state, "warm-ref", "B").await;
    assert_eq!(body["status"], json!("completed"));

    // Fallback run: retire the entry at each pause so /resume must
    // replay from the journal (the parked thread unwinds when its
    // resolution sender drops with the entry).
    let (_, body) = response_json(
        create_session(State(state.clone()), Json(warm_create_request("warm-fb"))).await,
    )
    .await;
    assert_eq!(body["status"], json!("paused"));
    state.warm_runs.lock().unwrap().remove("warm-fb");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let (_, body) = resume_with(&state, "warm-fb", "A").await;
    assert_eq!(body["status"], json!("paused"));
    state.warm_runs.lock().unwrap().remove("warm-fb");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let (status, body) = resume_with(&state, "warm-fb", "B").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], json!("completed"));
    assert_eq!(body["output"], json!({ "a": "A", "b": "B" }));

    let session = |id: &str| state.session_store.get(id).unwrap().unwrap();
    let warm_journal = journal_shape(&run_base, &session("warm-ref").run_id.unwrap());
    let fallback_journal = journal_shape(&run_base, &session("warm-fb").run_id.unwrap());
    assert_eq!(
        warm_journal, fallback_journal,
        "warm and replay resumes must write the same journal"
    );

    let _ = std::fs::remove_dir_all(temp_dir);
}

/// Eviction: a parked run that nobody resumes unwinds after the deadline
/// and the session remains resumable through the replay path (which then
/// re-upgrades the continuation onto the warm path).
#[tokio::test]
async fn warm_park_evicts_and_session_stays_resumable() {
    let temp_dir = std::env::temp_dir().join(format!(
        "chidori-server-warm-evict-{}",
        uuid::Uuid::new_v4()
    ));
    let agent_path = write_agent(&temp_dir, TWO_INPUT_AGENT);
    let run_base = temp_dir.join(".chidori").join("runs");
    std::fs::create_dir_all(&run_base).unwrap();
    let mut state = test_state(run_base.clone(), agent_path);
    state.warm_evict = std::time::Duration::from_millis(50);

    let (_, body) = response_json(
        create_session(State(state.clone()), Json(warm_create_request("warm-ev"))).await,
    )
    .await;
    assert_eq!(body["status"], json!("paused"));

    // Wait out the eviction deadline: the parked thread must unwind.
    let mut waited = 0;
    while warm_parked(&state, "warm-ev") && waited < 5_000 {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        waited += 25;
    }
    assert!(
        !warm_parked(&state, "warm-ev"),
        "parked run must evict after the deadline"
    );

    let (_, body) = resume_with(&state, "warm-ev", "A").await;
    assert_eq!(body["status"], json!("paused"));
    let (status, body) = resume_with(&state, "warm-ev", "B").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], json!("completed"));
    assert_eq!(body["output"], json!({ "a": "A", "b": "B" }));

    let _ = std::fs::remove_dir_all(temp_dir);
}

fn create_request(policy_profile: Option<&str>) -> CreateSessionRequest {
    CreateSessionRequest {
        input: json!({}),
        session_id: None,
        attempt_number: None,
        replay_from: None,
        agent: None,
        policy_profile: policy_profile.map(ToOwned::to_owned),
    }
}

const HTTP_AGENT: &str = r#"
    export async function agent(input, chidori) {
        const res = await fetch("https://example.invalid/");
        return { status: res.status };
    }
"#;

#[tokio::test]
async fn create_session_rejects_unknown_policy_profile() {
    let (temp_dir, state) = policy_test_project("chidori-server-policy-unknown", HTTP_AGENT);

    let (status, body) =
        response_json(create_session(State(state), Json(create_request(Some("nonsense")))).await)
            .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let error = body["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("unknown policy profile 'nonsense'") && error.contains("untrusted"),
        "expected an unknown-profile error listing the builtins, got: {error}"
    );

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[tokio::test]
async fn serve_default_profile_denies_sessions_and_explains_the_opt_out() {
    // The policy `chidori serve` resolves when the operator configured
    // nothing: sessions are deny-by-default and the denial tells the
    // operator how to relax the posture.
    let (temp_dir, mut state) =
        policy_test_project("chidori-server-policy-serve-default", HTTP_AGENT);
    state.policy = Arc::new(crate::policy::serve_default_profile());

    let (status, body) =
        response_json(create_session(State(state), Json(create_request(None))).await).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["status"], json!("failed"));
    let error = body["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("policy: `http` denied") && error.contains("--trusted"),
        "expected an actionable deny-by-default error, got: {error}"
    );

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[tokio::test]
async fn untrusted_session_denies_gated_effects_despite_permissive_server_policy() {
    let (temp_dir, state) = policy_test_project("chidori-server-policy-untrusted", HTTP_AGENT);
    // The server policy is the permissive default (AlwaysAllow); the
    // session profile must tighten it, not the other way around.

    let (status, body) =
        response_json(create_session(State(state), Json(create_request(Some("untrusted")))).await)
            .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["status"], json!("failed"));
    assert_eq!(body["policy_profile"], json!("untrusted"));
    let error = body["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("policy: `http` denied"),
        "expected the http call to be denied, got: {error}"
    );

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[tokio::test]
async fn supervised_session_pauses_for_approval_then_operator_denies() {
    let (temp_dir, state) = policy_test_project("chidori-server-policy-supervised", HTTP_AGENT);

    let (status, body) = response_json(
        create_session(
            State(state.clone()),
            Json(create_request(Some("supervised"))),
        )
        .await,
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["status"], json!("awaitingapproval"));
    assert_eq!(body["policy_profile"], json!("supervised"));
    assert_eq!(body["pending_approval"]["target"], json!("http"));
    let id = body["id"].as_str().unwrap().to_string();

    // Operator denies: the session fails without the call executing.
    let (status, body) = response_json(
        approve_session(
            State(state),
            Path(id),
            Json(ApproveRequest {
                decision: "deny".to_string(),
                allow_source_change: false,
            }),
        )
        .await,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], json!("failed"));
    let error = body["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("denied by operator"),
        "expected an operator-denied error, got: {error}"
    );

    let _ = std::fs::remove_dir_all(temp_dir);
}

// -----------------------------------------------------------------------
// Signal delivery (`docs/signals.md` §9–§11) — Stage 2.
// -----------------------------------------------------------------------

/// Build a state whose run_base is the agent's `.chidori/runs` dir (so a run
/// pausing on a signal persists into the same tree the delivery endpoint
/// reads), with a generous acquire timeout for the real engine runs.
fn signal_test_state(temp_dir: &FsPath, agent_path: PathBuf) -> AppState {
    let run_base = temp_dir.join(".chidori").join("runs");
    std::fs::create_dir_all(&run_base).unwrap();
    let mut state = test_state(run_base, agent_path);
    state.run_semaphore = Arc::new(Semaphore::new(4));
    state.acquire_timeout = std::time::Duration::from_secs(30);
    state
}

fn write_agent(temp_dir: &FsPath, source: &str) -> PathBuf {
    std::fs::create_dir_all(temp_dir).unwrap();
    let agent_path = temp_dir.join("agent.ts");
    std::fs::write(&agent_path, source).unwrap();
    agent_path
}

async fn create_paused_session(state: &AppState, id: &str, input: Value) -> Value {
    let (_status, body) = response_json(
        create_session(
            State(state.clone()),
            Json(
                serde_json::from_value(json!({
                    "input": input,
                    "session_id": id,
                }))
                .unwrap(),
            ),
        )
        .await,
    )
    .await;
    body
}

/// Signal delivered to a run paused waiting on THIS name resolves the pause
/// and resumes to completion; the final output and the session view reflect
/// the delivered payload.
#[tokio::test]
async fn signal_to_paused_waiting_this_name_resolves_and_resumes() {
    let temp_dir =
        std::env::temp_dir().join(format!("chidori-signal-resolve-{}", uuid::Uuid::new_v4()));
    let agent_path = write_agent(
        &temp_dir,
        r#"
            export async function agent(input, chidori) {
                const review = await chidori.signal("review");
                return { decision: review.payload.decision, by: review.from.id };
            }
        "#,
    );
    let state = signal_test_state(&temp_dir, agent_path);

    let created = create_paused_session(&state, "s-signal-1", json!({})).await;
    assert_eq!(created["status"], json!("paused"));
    assert_eq!(created["pending_signal_name"], json!("review"));

    let (status, body) = response_json(
        signal_session(
            State(state.clone()),
            Path("s-signal-1".to_string()),
            Json(SignalRequest {
                allow_source_change: false,
                name: "review".to_string(),
                payload: json!({ "decision": "approve" }),
                from: json!({ "kind": "human", "id": "mara" }),
            }),
        )
        .await,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], json!("completed"));
    assert_eq!(
        body["output"],
        json!({ "decision": "approve", "by": "mara" })
    );

    let stored = state.session_store.get("s-signal-1").unwrap().unwrap();
    assert_eq!(stored.status, SessionStatus::Completed);
    assert_eq!(stored.pending_signal_name, None);

    let _ = std::fs::remove_dir_all(temp_dir);
}

/// Signal delivered to a run paused on `input()` (a different pause) is
/// enqueued; the run stays paused; after the input is resumed, the agent's
/// later `signal(name)` drains the queued entry without pausing again.
#[tokio::test]
async fn signal_to_input_paused_enqueues_then_drains_after_resume() {
    let temp_dir =
        std::env::temp_dir().join(format!("chidori-signal-enqueue-{}", uuid::Uuid::new_v4()));
    let agent_path = write_agent(
        &temp_dir,
        r#"
            export async function agent(input, chidori) {
                const name = await chidori.input("who?");
                const review = await chidori.signal("review");
                return { name, decision: review.payload.decision };
            }
        "#,
    );
    let state = signal_test_state(&temp_dir, agent_path);

    let created = create_paused_session(&state, "s-signal-2", json!({})).await;
    assert_eq!(created["status"], json!("paused"));
    // This is an input() pause, not a signal pause.
    assert_eq!(created["pending_signal_name"], Value::Null);
    let run_id = created["run_id"].as_str().unwrap().to_string();

    // Deliver a "review" signal while paused on input → must enqueue.
    let (status, body) = response_json(
        signal_session(
            State(state.clone()),
            Path("s-signal-2".to_string()),
            Json(SignalRequest {
                allow_source_change: false,
                name: "review".to_string(),
                payload: json!({ "decision": "changes" }),
                from: json!({ "id": "bot" }),
            }),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], json!("queued"));
    assert_eq!(body["delivery_seq"], json!(1));

    // inbox.json exists with the entry, and the run is still paused.
    let inbox = load_persisted_signal_inbox(&state.run_base, Some(&run_id));
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].name, "review");
    let still_paused = state.session_store.get("s-signal-2").unwrap().unwrap();
    assert_eq!(still_paused.status, SessionStatus::Paused);

    // Resume the input(); the later signal() drains the queued entry.
    let (status, body) = response_json(
        resume_session(
            State(state.clone()),
            Path("s-signal-2".to_string()),
            Json(ResumeRequest {
                response: "ada".to_string(),
                allow_source_change: false,
            }),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], json!("completed"));
    assert_eq!(
        body["output"],
        json!({ "name": "ada", "decision": "changes" })
    );

    // Inbox was drained to empty by consumption.
    let drained = load_persisted_signal_inbox(&state.run_base, Some(&run_id));
    assert!(
        drained.is_empty(),
        "inbox should be drained, got {drained:?}"
    );

    let _ = std::fs::remove_dir_all(temp_dir);
}

/// Signal delivered to a completed run → 409 Conflict and NO inbox file.
#[tokio::test]
async fn signal_to_completed_run_conflicts_with_no_inbox() {
    let temp_dir =
        std::env::temp_dir().join(format!("chidori-signal-409-{}", uuid::Uuid::new_v4()));
    let agent_path = write_agent(
        &temp_dir,
        r#"
            export async function agent() { return { ok: true }; }
        "#,
    );
    let state = signal_test_state(&temp_dir, agent_path);

    let created = create_paused_session(&state, "s-signal-3", json!({})).await;
    assert_eq!(created["status"], json!("completed"));
    let run_id = created["run_id"].as_str().unwrap().to_string();

    let (status, _body) = response_json(
        signal_session(
            State(state.clone()),
            Path("s-signal-3".to_string()),
            Json(SignalRequest {
                name: "review".to_string(),
                payload: Value::Null,
                from: Value::Null,
                allow_source_change: false,
            }),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let inbox_path = state
        .run_base
        .join(&run_id)
        .join(crate::runtime::snapshot::SIGNAL_INBOX_FILE);
    assert!(
        !inbox_path.exists(),
        "completed run must not have an inbox written"
    );

    let _ = std::fs::remove_dir_all(temp_dir);
}

/// Determinism: enqueue-before-listen and pause-then-deliver produce the
/// identical recorded `signal` CallRecord and final output. Both paths use
/// the same agent body and deliver the same `{name,payload,from}`; only the
/// arrival timing differs (queued-then-drained vs pending-pause-resolved).
#[tokio::test]
async fn signal_enqueue_before_listen_matches_pause_then_deliver() {
    let payload = json!({ "decision": "approve" });
    let from = json!({ "kind": "human", "id": "mara" });

    // Path A: pause-then-deliver. The agent reaches `signal("review")` with
    // an empty mailbox, pauses, and the delivery resolves the pending pause.
    let source_a = r#"
        export async function agent(input, chidori) {
            const review = await chidori.signal("review");
            return { decision: review.payload.decision, by: review.from.id };
        }
    "#;
    let dir_a = std::env::temp_dir().join(format!("chidori-signal-det-a-{}", uuid::Uuid::new_v4()));
    let agent_a = write_agent(&dir_a, source_a);
    let state_a = signal_test_state(&dir_a, agent_a);
    create_paused_session(&state_a, "a", json!({})).await;
    response_json(
        signal_session(
            State(state_a.clone()),
            Path("a".to_string()),
            Json(SignalRequest {
                name: "review".to_string(),
                payload: payload.clone(),
                from: from.clone(),
                allow_source_change: false,
            }),
        )
        .await,
    )
    .await;
    let stored_a = state_a.session_store.get("a").unwrap().unwrap();

    // Path B: enqueue-before-listen. The agent first pauses on `input()`; we
    // deliver "review" while it is paused (so the signal is ENQUEUED, never a
    // pending-pause), then resume the input(). When the agent reaches
    // `signal("review")` it drains the queued entry WITHOUT pausing. The
    // recorded signal value must be identical to Path A.
    let source_b = r#"
        export async function agent(input, chidori) {
            await chidori.input("gate");
            const review = await chidori.signal("review");
            return { decision: review.payload.decision, by: review.from.id };
        }
    "#;
    let dir_b = std::env::temp_dir().join(format!("chidori-signal-det-b-{}", uuid::Uuid::new_v4()));
    let agent_b = write_agent(&dir_b, source_b);
    let state_b = signal_test_state(&dir_b, agent_b);
    create_paused_session(&state_b, "b", json!({})).await;
    // Deliver while paused on input → enqueued.
    let (qstatus, _) = response_json(
        signal_session(
            State(state_b.clone()),
            Path("b".to_string()),
            Json(SignalRequest {
                name: "review".to_string(),
                payload: payload.clone(),
                from: from.clone(),
                allow_source_change: false,
            }),
        )
        .await,
    )
    .await;
    assert_eq!(qstatus, StatusCode::ACCEPTED);
    // Resume the input(); the later signal() drains the queued entry.
    response_json(
        resume_session(
            State(state_b.clone()),
            Path("b".to_string()),
            Json(ResumeRequest {
                response: "go".to_string(),
                allow_source_change: false,
            }),
        )
        .await,
    )
    .await;
    let stored_b = state_b.session_store.get("b").unwrap().unwrap();

    // Identical recorded signal CallRecord (result + match-key args) and
    // identical final output, regardless of arrival timing.
    let sig_a = stored_a
        .call_log
        .iter()
        .find(|r| r.function == "signal")
        .unwrap();
    let sig_b = stored_b
        .call_log
        .iter()
        .find(|r| r.function == "signal")
        .unwrap();
    assert_eq!(sig_a.result, sig_b.result);
    assert_eq!(sig_a.args, sig_b.args);
    assert_eq!(stored_a.output, stored_b.output);

    let _ = std::fs::remove_dir_all(dir_a);
    let _ = std::fs::remove_dir_all(dir_b);
}

/// Tie-break (`docs/signals.md` §11, pinned "pending-pause-wins-with-newest"):
/// a queued same-name entry already in the inbox PLUS a pending pause on that
/// name PLUS a new delivery → the pause resolves with the NEW payload, and
/// the older queued entry survives for a later listen point.
#[tokio::test]
async fn signal_tie_break_pending_pause_wins_with_newest() {
    let temp_dir =
        std::env::temp_dir().join(format!("chidori-signal-tiebreak-{}", uuid::Uuid::new_v4()));
    // Two sequential review listen points: the first consumes the new
    // delivery (pause wins), the second drains the older queued entry.
    let agent_path = write_agent(
        &temp_dir,
        r#"
            export async function agent(input, chidori) {
                const first = await chidori.signal("review");
                const second = await chidori.signal("review");
                return { first: first.payload.tag, second: second.payload.tag };
            }
        "#,
    );
    let state = signal_test_state(&temp_dir, agent_path);

    let created = create_paused_session(&state, "tie", json!({})).await;
    let run_id = created["run_id"].as_str().unwrap().to_string();
    assert_eq!(created["pending_signal_name"], json!("review"));

    // Pre-seed an OLDER queued same-name entry directly into the inbox while
    // the run is paused on the first `review`.
    enqueue_signal_to_inbox(
        &state,
        &run_id,
        "review",
        json!({ "tag": "old-queued" }),
        json!({ "id": "queued-sender" }),
    )
    .unwrap();

    // Deliver a NEW review: the pending pause must resolve with THIS one.
    let (status, body) = response_json(
        signal_session(
            State(state.clone()),
            Path("tie".to_string()),
            Json(SignalRequest {
                allow_source_change: false,
                name: "review".to_string(),
                payload: json!({ "tag": "new-delivered" }),
                from: json!({ "id": "live-sender" }),
            }),
        )
        .await,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], json!("completed"));
    // First listen point got the NEW payload; second drained the OLD queued.
    assert_eq!(
        body["output"],
        json!({ "first": "new-delivered", "second": "old-queued" })
    );

    let _ = std::fs::remove_dir_all(temp_dir);
}

/// End-to-end: an agent with two sequential `signal("review")` calls; two
/// deliveries; both recorded in order with their delivered payloads.
#[tokio::test]
async fn signal_two_sequential_listen_points_record_in_order() {
    let temp_dir =
        std::env::temp_dir().join(format!("chidori-signal-two-{}", uuid::Uuid::new_v4()));
    let agent_path = write_agent(
        &temp_dir,
        r#"
            export async function agent(input, chidori) {
                const a = await chidori.signal("review");
                const b = await chidori.signal("review");
                return { a: a.payload.round, b: b.payload.round };
            }
        "#,
    );
    let state = signal_test_state(&temp_dir, agent_path);

    create_paused_session(&state, "two", json!({})).await;

    // First delivery resolves the first listen point; the run re-pauses on
    // the second `review`.
    let (status, body) = response_json(
        signal_session(
            State(state.clone()),
            Path("two".to_string()),
            Json(SignalRequest {
                allow_source_change: false,
                name: "review".to_string(),
                payload: json!({ "round": 1 }),
                from: json!({ "id": "r1" }),
            }),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], json!("paused"));
    assert_eq!(body["pending_signal_name"], json!("review"));

    // Second delivery resolves the second listen point → completion.
    let (status, body) = response_json(
        signal_session(
            State(state.clone()),
            Path("two".to_string()),
            Json(SignalRequest {
                allow_source_change: false,
                name: "review".to_string(),
                payload: json!({ "round": 2 }),
                from: json!({ "id": "r2" }),
            }),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], json!("completed"));
    assert_eq!(body["output"], json!({ "a": 1, "b": 2 }));

    // Both signal records present, in seq order, with their payloads.
    let stored = state.session_store.get("two").unwrap().unwrap();
    let signals: Vec<_> = stored
        .call_log
        .iter()
        .filter(|r| r.function == "signal")
        .collect();
    assert_eq!(signals.len(), 2);
    assert!(signals[0].seq < signals[1].seq);
    assert_eq!(signals[0].result["payload"], json!({ "round": 1 }));
    assert_eq!(signals[1].result["payload"], json!({ "round": 2 }));

    let _ = std::fs::remove_dir_all(temp_dir);
}

/// Pure replay (`/sessions/{id}/replay`) must NOT consume the inbox: a replay
/// with a non-empty inbox leaves the inbox file unchanged and reproduces the
/// identical output (the recorded `signal` call short-circuits before the
/// mailbox drain — the determinism contract, `docs/signals.md` §10).
#[tokio::test]
async fn replay_does_not_consume_inbox() {
    let temp_dir =
        std::env::temp_dir().join(format!("chidori-signal-replay-{}", uuid::Uuid::new_v4()));
    let agent_path = write_agent(
        &temp_dir,
        r#"
            export async function agent(input, chidori) {
                const review = await chidori.signal("review");
                return { decision: review.payload.decision };
            }
        "#,
    );
    let state = signal_test_state(&temp_dir, agent_path);

    // Drive a run to completion via a delivered signal.
    let created = create_paused_session(&state, "replay-src", json!({})).await;
    let run_id = created["run_id"].as_str().unwrap().to_string();
    response_json(
        signal_session(
            State(state.clone()),
            Path("replay-src".to_string()),
            Json(SignalRequest {
                allow_source_change: false,
                name: "review".to_string(),
                payload: json!({ "decision": "approve" }),
                from: json!({ "id": "x" }),
            }),
        )
        .await,
    )
    .await;
    let completed = state.session_store.get("replay-src").unwrap().unwrap();
    assert_eq!(completed.status, SessionStatus::Completed);

    // Seed a non-empty inbox under the run dir; replay must ignore it.
    enqueue_signal_to_inbox(
        &state,
        &run_id,
        "review",
        json!({ "decision": "SHOULD-NOT-BE-USED" }),
        json!({ "id": "ghost" }),
    )
    .unwrap();
    let inbox_before = load_persisted_signal_inbox(&state.run_base, Some(&run_id));
    assert_eq!(inbox_before.len(), 1);

    let (status, body) =
        response_json(replay_session(State(state.clone()), Path("replay-src".to_string())).await)
            .await;
    assert_eq!(status, StatusCode::CREATED);
    // Replay reproduces the recorded decision, not the ghost inbox entry.
    assert_eq!(body["output"], json!({ "decision": "approve" }));

    // Inbox file is untouched by replay.
    let inbox_after = load_persisted_signal_inbox(&state.run_base, Some(&run_id));
    assert_eq!(inbox_after, inbox_before);

    let _ = std::fs::remove_dir_all(temp_dir);
}

// -----------------------------------------------------------------------
// Phase 2: signalAny + timeoutMs (`docs/signals.md` §14 Phase 2).
// -----------------------------------------------------------------------

/// Poll the session store until `id` reaches `status` (the timeout timers
/// and streaming supervisor advance sessions asynchronously).
async fn wait_for_status(state: &AppState, id: &str, status: SessionStatus) -> StoredSession {
    for _ in 0..400 {
        if let Ok(Some(s)) = state.session_store.get(id) {
            if s.status == status {
                return s;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("session {id} never reached {status:?}");
}

/// `chidori.signal([..])` pauses on the whole listen set, advertises it
/// in the session view, and a delivery matching ANY listed name resolves
/// the pause; the recorded call replays as `signal_any` with its `{names}`
/// match key and the bare fired signal as result.
#[tokio::test]
async fn signal_any_pauses_on_set_and_resolves_on_any_listed_name() {
    let temp_dir =
        std::env::temp_dir().join(format!("chidori-signal-any-{}", uuid::Uuid::new_v4()));
    let agent_path = write_agent(
        &temp_dir,
        r#"
            export async function agent(input, chidori) {
                const fired = await chidori.signal(["review", "steer"]);
                return { fired: fired.name, payload: fired.payload, by: fired.from.id };
            }
        "#,
    );
    let state = signal_test_state(&temp_dir, agent_path);

    let created = create_paused_session(&state, "any-1", json!({})).await;
    assert_eq!(created["status"], json!("paused"));
    assert_eq!(created["pending_signal_name"], json!("review"));
    assert_eq!(created["pending_signal_names"], json!(["review", "steer"]));

    // Deliver the SECOND listed name — it must resolve the pause.
    let (status, body) = response_json(
        signal_session(
            State(state.clone()),
            Path("any-1".to_string()),
            Json(SignalRequest {
                allow_source_change: false,
                name: "steer".to_string(),
                payload: json!({ "dir": "left" }),
                from: json!({ "kind": "human", "id": "sam" }),
            }),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], json!("completed"));
    assert_eq!(
        body["output"],
        json!({ "fired": "steer", "payload": { "dir": "left" }, "by": "sam" })
    );

    // The synthetic record uses the persisted op's function + match key.
    let stored = state.session_store.get("any-1").unwrap().unwrap();
    let record = stored
        .call_log
        .iter()
        .find(|r| r.function == "signal_any")
        .expect("signal_any record");
    assert_eq!(record.args, json!({ "names": ["review", "steer"] }));
    assert_eq!(record.result["name"], json!("steer"));

    let _ = std::fs::remove_dir_all(temp_dir);
}

/// A `timeoutMs` signal pause persists its deadline, and the armed server
/// timer resolves it with the `{timedOut: true}` sentinel; a replay of the
/// timed-out session reproduces the sentinel from the recorded call.
#[tokio::test]
async fn signal_timeout_resolves_with_sentinel_and_replays() {
    let temp_dir =
        std::env::temp_dir().join(format!("chidori-signal-timeout-{}", uuid::Uuid::new_v4()));
    let agent_path = write_agent(
        &temp_dir,
        r#"
            export async function agent(input, chidori) {
                const result = await chidori.signal("review", { timeoutMs: 150 });
                if (result.timedOut) {
                    return { timedOut: true, name: result.name };
                }
                return { timedOut: false, decision: result.payload.decision };
            }
        "#,
    );
    let state = signal_test_state(&temp_dir, agent_path);

    let created = create_paused_session(&state, "to-1", json!({})).await;
    assert_eq!(created["status"], json!("paused"));
    assert!(
        !created["pending_signal_deadline"].is_null(),
        "timeoutMs pause must persist its deadline: {created}"
    );

    // No delivery: the armed timer fires and resolves the sentinel.
    let stored = wait_for_status(&state, "to-1", SessionStatus::Completed).await;
    assert_eq!(
        stored.output,
        Some(json!({ "timedOut": true, "name": "review" }))
    );
    let record = stored
        .call_log
        .iter()
        .find(|r| r.function == "signal")
        .expect("signal record");
    assert_eq!(record.result["timedOut"], json!(true));

    // Replay reproduces the sentinel deterministically.
    let (status, body) =
        response_json(replay_session(State(state.clone()), Path("to-1".to_string())).await).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        body["output"],
        json!({ "timedOut": true, "name": "review" })
    );

    let _ = std::fs::remove_dir_all(temp_dir);
}

/// A delivery that lands before the `timeoutMs` deadline wins; the timer
/// later fires as a no-op (the pause is already resolved).
#[tokio::test]
async fn signal_delivery_before_timeout_wins() {
    let temp_dir =
        std::env::temp_dir().join(format!("chidori-signal-race-{}", uuid::Uuid::new_v4()));
    let agent_path = write_agent(
        &temp_dir,
        r#"
            export async function agent(input, chidori) {
                const result = await chidori.signal("review", { timeoutMs: 60000 });
                if (result.timedOut) {
                    return { timedOut: true };
                }
                return { timedOut: false, decision: result.payload.decision };
            }
        "#,
    );
    let state = signal_test_state(&temp_dir, agent_path);

    create_paused_session(&state, "race-1", json!({})).await;
    let (status, body) = response_json(
        signal_session(
            State(state.clone()),
            Path("race-1".to_string()),
            Json(SignalRequest {
                allow_source_change: false,
                name: "review".to_string(),
                payload: json!({ "decision": "approve" }),
                from: json!({ "id": "mara" }),
            }),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["output"],
        json!({ "timedOut": false, "decision": "approve" })
    );

    let _ = std::fs::remove_dir_all(temp_dir);
}

/// A timed-out multi-name `signalAny` resolves to the sentinel with a null
/// `name` (no name fired).
#[tokio::test]
async fn signal_any_timeout_sentinel_has_null_name() {
    let temp_dir =
        std::env::temp_dir().join(format!("chidori-signal-any-to-{}", uuid::Uuid::new_v4()));
    let agent_path = write_agent(
        &temp_dir,
        r#"
            export async function agent(input, chidori) {
                const result = await chidori.signal(["a", "b"], { timeoutMs: 150 });
                return { timedOut: result.timedOut === true, name: result.name };
            }
        "#,
    );
    let state = signal_test_state(&temp_dir, agent_path);

    let created = create_paused_session(&state, "any-to", json!({})).await;
    assert_eq!(created["pending_signal_names"], json!(["a", "b"]));

    let stored = wait_for_status(&state, "any-to", SessionStatus::Completed).await;
    assert_eq!(
        stored.output,
        Some(json!({ "timedOut": true, "name": null }))
    );

    let _ = std::fs::remove_dir_all(temp_dir);
}

// -----------------------------------------------------------------------
// Phase 3: live in-memory delivery to streaming runs (`docs/signals.md`).
// -----------------------------------------------------------------------

/// Drive a streaming session through `stream_session` and collect its full
/// SSE body in a background task (the body future drives the supervisor).
async fn start_stream(
    state: &AppState,
    session_id: &str,
    input: Value,
) -> tokio::task::JoinHandle<String> {
    let response = stream_session(
        State(state.clone()),
        Json(
            serde_json::from_value(json!({
                "input": input,
                "session_id": session_id,
            }))
            .unwrap(),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    tokio::spawn(async move {
        let bytes = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).to_string()
    })
}

/// A streaming run that pauses on `signal()` stays live: the delivery
/// endpoint reports `delivered_live` and the supervisor resolves the pause
/// in-process (no `/resume` round-trip), the stream carrying a `paused`
/// event and then `done`. The recorded signal call matches the durable
/// persist-resume path byte-for-byte on its match key and result.
#[tokio::test]
async fn stream_session_resolves_signal_pause_in_process() {
    let temp_dir =
        std::env::temp_dir().join(format!("chidori-stream-signal-{}", uuid::Uuid::new_v4()));
    let source = r#"
        export async function agent(input, chidori) {
            const review = await chidori.signal("review");
            return { decision: review.payload.decision, by: review.from.id };
        }
    "#;
    let agent_path = write_agent(&temp_dir, source);
    let state = signal_test_state(&temp_dir, agent_path);

    let sse = start_stream(&state, "live-1", json!({})).await;

    // Wait for the worker to persist the supervised signal pause; the
    // session must STAY in active_sessions (live supervision continues).
    let paused = wait_for_status(&state, "live-1", SessionStatus::Paused).await;
    assert_eq!(paused.pending_signal_name.as_deref(), Some("review"));
    assert!(state.active_sessions.lock().unwrap().contains_key("live-1"));

    // Deliver: routed to the live worker, not the durable resume path.
    let (status, body) = response_json(
        signal_session(
            State(state.clone()),
            Path("live-1".to_string()),
            Json(SignalRequest {
                allow_source_change: false,
                name: "review".to_string(),
                payload: json!({ "decision": "approve" }),
                from: json!({ "kind": "human", "id": "mara" }),
            }),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], json!("delivered_live"));

    // The supervisor resumes in-process and completes the session.
    let stored = wait_for_status(&state, "live-1", SessionStatus::Completed).await;
    assert_eq!(
        stored.output,
        Some(json!({ "decision": "approve", "by": "mara" }))
    );
    assert!(!state.active_sessions.lock().unwrap().contains_key("live-1"));

    // The stream announced the pause and finished with done/completed.
    let sse_text = sse.await.unwrap();
    assert!(sse_text.contains("event: paused"), "sse: {sse_text}");
    assert!(sse_text.contains("event: done"), "sse: {sse_text}");
    assert!(
        sse_text.contains("\"status\":\"completed\""),
        "sse: {sse_text}"
    );
    // The consumed signal itself must ride the stream as a `call` event —
    // it is the one record carrying `{name, payload, from}`, and a
    // dashboard cannot show who steered the run without it (the resumed
    // run's replayed records deliberately re-emit nothing).
    assert!(
        sse_text.contains("\"function\":\"signal\""),
        "sse must carry the consumed signal record: {sse_text}"
    );
    assert!(
        sse_text.contains("\"id\":\"mara\""),
        "sse signal record must carry sender provenance: {sse_text}"
    );

    // Determinism: the same agent driven through the durable
    // create→deliver path records the identical signal call.
    let dir_b =
        std::env::temp_dir().join(format!("chidori-stream-signal-b-{}", uuid::Uuid::new_v4()));
    let agent_b = write_agent(&dir_b, source);
    let state_b = signal_test_state(&dir_b, agent_b);
    create_paused_session(&state_b, "durable-1", json!({})).await;
    response_json(
        signal_session(
            State(state_b.clone()),
            Path("durable-1".to_string()),
            Json(SignalRequest {
                allow_source_change: false,
                name: "review".to_string(),
                payload: json!({ "decision": "approve" }),
                from: json!({ "kind": "human", "id": "mara" }),
            }),
        )
        .await,
    )
    .await;
    let durable = state_b.session_store.get("durable-1").unwrap().unwrap();
    let sig_live = stored
        .call_log
        .iter()
        .find(|r| r.function == "signal")
        .unwrap();
    let sig_durable = durable
        .call_log
        .iter()
        .find(|r| r.function == "signal")
        .unwrap();
    assert_eq!(sig_live.args, sig_durable.args);
    assert_eq!(sig_live.result, sig_durable.result);
    assert_eq!(sig_live.seq, sig_durable.seq);
    assert_eq!(stored.output, durable.output);

    let _ = std::fs::remove_dir_all(temp_dir);
    let _ = std::fs::remove_dir_all(dir_b);
}

/// A signal delivered live for a name the run is NOT waiting on lands in
/// the live in-memory mailbox (write-through persisted) and survives the
/// in-process resume: the resumed run drains it at a later `pollSignal`.
#[tokio::test]
async fn stream_session_live_mailbox_carries_over_in_process_resume() {
    let temp_dir =
        std::env::temp_dir().join(format!("chidori-stream-mailbox-{}", uuid::Uuid::new_v4()));
    let agent_path = write_agent(
        &temp_dir,
        r#"
            export async function agent(input, chidori) {
                const review = await chidori.signal("review");
                const steer = await chidori.pollSignal("steer");
                return {
                    decision: review.payload.decision,
                    steer: steer ? steer.payload.dir : null,
                };
            }
        "#,
    );
    let state = signal_test_state(&temp_dir, agent_path);

    let sse = start_stream(&state, "live-2", json!({})).await;
    wait_for_status(&state, "live-2", SessionStatus::Paused).await;

    // Deliver a NON-matching name first: enqueued live, no resume.
    let (status, body) = response_json(
        signal_session(
            State(state.clone()),
            Path("live-2".to_string()),
            Json(SignalRequest {
                allow_source_change: false,
                name: "steer".to_string(),
                payload: json!({ "dir": "left" }),
                from: json!({ "id": "sam" }),
            }),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], json!("delivered_live"));

    // Now resolve the supervised pause; the resumed run must still see the
    // queued "steer" at its pollSignal.
    response_json(
        signal_session(
            State(state.clone()),
            Path("live-2".to_string()),
            Json(SignalRequest {
                allow_source_change: false,
                name: "review".to_string(),
                payload: json!({ "decision": "approve" }),
                from: json!({ "id": "mara" }),
            }),
        )
        .await,
    )
    .await;

    let stored = wait_for_status(&state, "live-2", SessionStatus::Completed).await;
    assert_eq!(
        stored.output,
        Some(json!({ "decision": "approve", "steer": "left" }))
    );
    let _ = sse.await.unwrap();

    let _ = std::fs::remove_dir_all(temp_dir);
}

/// A streaming run paused with `timeoutMs` resolves to the sentinel from
/// the supervisor's own deadline (no external delivery, stream stays one
/// continuous session through to done).
#[tokio::test]
async fn stream_session_signal_timeout_resolves_in_process() {
    let temp_dir =
        std::env::temp_dir().join(format!("chidori-stream-timeout-{}", uuid::Uuid::new_v4()));
    let agent_path = write_agent(
        &temp_dir,
        r#"
            export async function agent(input, chidori) {
                const result = await chidori.signal("review", { timeoutMs: 150 });
                return { timedOut: result.timedOut === true };
            }
        "#,
    );
    let state = signal_test_state(&temp_dir, agent_path);

    let sse = start_stream(&state, "live-3", json!({})).await;
    let stored = wait_for_status(&state, "live-3", SessionStatus::Completed).await;
    assert_eq!(stored.output, Some(json!({ "timedOut": true })));
    let sse_text = sse.await.unwrap();
    assert!(sse_text.contains("event: paused"), "sse: {sse_text}");
    assert!(
        sse_text.contains("\"status\":\"completed\""),
        "sse: {sse_text}"
    );

    let _ = std::fs::remove_dir_all(temp_dir);
}

// ---------------------------------------------------------------------------
// Round-5 review regressions: boot re-arm of signal deadlines, and the
// event-driven surface's handling of pausing runs.
// ---------------------------------------------------------------------------

/// A signal-pause deadline persisted by a server that died must fire after a
/// restart: the boot loop (`run()` in mod.rs) re-arms `arm_signal_timeout`
/// for every stored session, and an already-expired deadline resolves to the
/// timeout sentinel immediately. Documented in `docs/signals.md` ("re-armed
/// for every paused session at server startup").
#[tokio::test]
async fn signal_timeout_rearm_fires_for_deadline_persisted_by_a_dead_server() {
    let temp_dir =
        std::env::temp_dir().join(format!("chidori-signal-rearm-{}", uuid::Uuid::new_v4()));
    let agent_path = write_agent(
        &temp_dir,
        r#"
            export async function agent(input, chidori) {
                const result = await chidori.signal("review", { timeoutMs: 60000 });
                return { timedOut: result.timedOut === true };
            }
        "#,
    );
    let state = signal_test_state(&temp_dir, agent_path.clone());

    // Pause with a distant deadline (the first server's own timer is armed
    // for 60s — far past this test), then simulate that server dying and a
    // replacement booting AFTER the deadline: rewrite the persisted deadline
    // into the past, as wall clock passing would have.
    create_paused_session(&state, "rearm-1", json!({})).await;
    let mut stored = state.session_store.get("rearm-1").unwrap().unwrap();
    assert!(stored.pending_signal_deadline.is_some());
    stored.pending_signal_deadline = Some(chrono::Utc::now() - chrono::Duration::seconds(5));
    state.session_store.put(&stored).unwrap();

    // "Restart": a fresh AppState (empty active_sessions, new semaphore)
    // over the SAME session store and run directory, running the same boot
    // re-arm loop the server's startup runs.
    let mut restarted = test_state(temp_dir.join(".chidori").join("runs"), agent_path);
    restarted.session_store = state.session_store.clone();
    restarted.run_semaphore = Arc::new(Semaphore::new(4));
    restarted.acquire_timeout = std::time::Duration::from_secs(30);
    for session in restarted.session_store.list().unwrap() {
        arm_signal_timeout(&restarted, &session);
    }

    let stored = wait_for_status(&restarted, "rearm-1", SessionStatus::Completed).await;
    assert_eq!(stored.output, Some(json!({ "timedOut": true })));

    let _ = std::fs::remove_dir_all(temp_dir);
}

/// An event-driven run (`ANY /*`) that pauses at a signal listen point must
/// become a real, deliverable session: 202 Accepted carrying the session
/// view (id + pending names), not a bare `null` that strands the journaled
/// run. Delivering the signal then completes it.
#[tokio::test]
async fn event_run_that_pauses_becomes_a_deliverable_session() {
    let temp_dir =
        std::env::temp_dir().join(format!("chidori-event-pause-{}", uuid::Uuid::new_v4()));
    let agent_path = write_agent(
        &temp_dir,
        r#"
            export async function agent(input, chidori) {
                if (!input.event) return { status: 400, body: { error: "no event" } };
                const go = await chidori.signal("go");
                return { done: true, via: input.event.path, by: go.from.id };
            }
        "#,
    );
    let state = signal_test_state(&temp_dir, agent_path);

    let response = handle_event(
        State(state.clone()),
        axum::http::Method::POST,
        "/alerts/pagerduty".parse().unwrap(),
        axum::http::HeaderMap::new(),
        axum::extract::Query(HashMap::new()),
        axum::body::Bytes::from_static(b"{\"alert\":\"redis down\"}"),
    )
    .await;
    let (status, body) = response_json(response).await;
    assert_eq!(status, StatusCode::ACCEPTED, "paused event run: {body}");
    assert_eq!(body["status"], json!("paused"));
    assert_eq!(body["pending_signal_name"], json!("go"));
    let id = body["id"].as_str().expect("session id").to_string();
    assert!(
        state.session_store.get(&id).unwrap().is_some(),
        "paused event run must be stored as a session"
    );

    let (status, body) = response_json(
        signal_session(
            State(state.clone()),
            Path(id),
            Json(SignalRequest {
                allow_source_change: false,
                name: "go".to_string(),
                payload: json!({}),
                from: json!({ "kind": "human", "id": "dana" }),
            }),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["output"],
        json!({ "done": true, "via": "/alerts/pagerduty", "by": "dana" })
    );

    let _ = std::fs::remove_dir_all(temp_dir);
}

/// An event-driven run that completes stays stateless: the response mapping
/// (`{status, body}` honored as the HTTP response) is unchanged, and no
/// session row is stored — stray probes must not grow the session store.
#[tokio::test]
async fn event_run_that_completes_stays_stateless() {
    let temp_dir =
        std::env::temp_dir().join(format!("chidori-event-complete-{}", uuid::Uuid::new_v4()));
    let agent_path = write_agent(
        &temp_dir,
        r#"
            export async function agent(input) {
                if (!input.event || !input.event.body || !input.event.body.alert) {
                    return { status: 400, body: { error: "no alert in request" } };
                }
                return { status: 200, body: { ok: true } };
            }
        "#,
    );
    let state = signal_test_state(&temp_dir, agent_path);

    let response = handle_event(
        State(state.clone()),
        axum::http::Method::GET,
        "/favicon.ico".parse().unwrap(),
        axum::http::HeaderMap::new(),
        axum::extract::Query(HashMap::new()),
        axum::body::Bytes::new(),
    )
    .await;
    let (status, body) = response_json(response).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], json!("no alert in request"));
    assert!(
        state.session_store.list().unwrap().is_empty(),
        "completed event runs must not store sessions"
    );

    let _ = std::fs::remove_dir_all(temp_dir);
}
