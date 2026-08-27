//! Mainline pause imaging — `docs/resume-performance.md` §5.2.
//!
//! The mainline agent loop dispatches host effects synchronously and turns a
//! pause into an error unwind (`PAUSE_MARKER`), which tears the engine down
//! with the Rust stack: there is no suspended VM at a pause, so there is
//! nothing to image and every resume is O(history) re-execution.
//!
//! Behind `CHIDORI_MAINLINE_IMAGE` (default OFF) the *pause-capable* effects
//! are routed through the engine's async host-promise path instead
//! (`Vm::effect_suspend`): the effect yields a promise nothing resolves, the
//! awaiting frame parks, and the driver stops at a quiescent point where a VM
//! image can be taken. The image is written beside the existing paused
//! artifact — it never replaces it. The journal stays the source of truth: an
//! image that is absent, stale, or inapplicable costs a slower resume and
//! nothing else.
//!
//! Scope is deliberately narrow (see [`SUSPENDABLE_PAUSE_EFFECTS`]): only
//! effects that pause get converted. Converting the rest would move their
//! results across a microtask boundary and change the interleaving replay
//! depends on.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::runtime::call_log::CallRecord;
use crate::runtime::context::RuntimeContext;
use crate::runtime::errors::RunInterrupt;

/// Auxiliary blob the image rides in, under the run's store. Additive: nothing
/// else reads it, and every reader that predates it resumes by replay.
pub(crate) const IMAGE_BLOB: &str = "vm_image.json";

/// Bumped whenever the envelope below changes shape. A mismatch is a decline,
/// not an error.
const ENVELOPE_VERSION: u32 = 1;

/// The effects whose host-side pause is converted into a VM suspension.
///
/// `input` only, for now. It is the dominant production pause (§5), and its
/// resume is a plain value delivery — the pending operation is already durable
/// before the pause, and the resume path already injects a synthetic record
/// carrying the answer. Approval and signal/alarm pauses keep the classic
/// unwind: their delivery paths (policy decisions, the signal mailbox and its
/// timeout arming) re-derive state on the re-execution they still perform, so
/// converting them is a separate change, not a wider list here.
pub(crate) const SUSPENDABLE_PAUSE_EFFECTS: &[&str] = &["input"];

/// Whether mainline pause imaging is on (`CHIDORI_MAINLINE_IMAGE`, default
/// off). This is a scoped redesign of the pause path, so it is opt-in: with the
/// flag unset the pause unwinds exactly as it always has and no image is
/// written or read.
pub(crate) fn enabled() -> bool {
    matches!(
        std::env::var("CHIDORI_MAINLINE_IMAGE").as_deref(),
        Ok("1") | Ok("true") | Ok("on")
    )
}

/// Whether `err` — a host dispatch failure string — is a pause of an effect
/// this mechanism converts. Anything else stays a thrown JS error.
pub(crate) fn is_suspendable_pause(effect: &str, err: &str) -> bool {
    SUSPENDABLE_PAUSE_EFFECTS.contains(&effect)
        && matches!(
            RunInterrupt::from_message(err),
            Some(RunInterrupt::Input { .. })
        )
}

/// A mainline VM image plus everything needed to decide whether it still
/// applies to the run it is being asked to continue.
#[derive(Serialize, Deserialize)]
pub(crate) struct MainlineImage {
    version: u32,
    /// Entry module key (its canonical path) and a hash of its transpiled
    /// source. The image's own per-unit digests catch a changed *imported*
    /// module; this catches a changed entry before any of that runs.
    entry_key: String,
    entry_hash: String,
    /// Journal frontier at the pause. The image describes the program exactly
    /// after these records, so it applies only to a resume carrying this
    /// prefix plus the one delivered record.
    call_log_len: usize,
    /// The parked effect and the host op the restored VM resolves.
    effect: String,
    op_id: u64,
    /// Sequence number of the pending host operation the pause left behind.
    pending_seq: u64,
    vm: chidori_js::image::VmImage,
}

/// What a resume needs once an image has been accepted.
pub(crate) struct AcceptedImage {
    pub(crate) image: chidori_js::image::VmImage,
    pub(crate) op_id: u64,
    /// The delivered value the parked host op resolves with — the result of the
    /// synthetic record the resume path injected at the pending seq.
    pub(crate) delivered: Value,
}

fn entry_hash(source: &str) -> String {
    crate::runtime::snapshot::SourceFingerprint::from_source("<entry>", source).hash
}

/// Capture the quiescent VM as an image and write it beside the paused
/// artifact. Best-effort by construction: every failure leaves the run's
/// durable state exactly as the unwinding pause path would have, so the caller
/// ignores the result beyond logging.
pub(crate) fn capture(
    ctx: &RuntimeContext,
    engine: &chidori_js::Engine,
    entry_key: &str,
    entry_src: &str,
) {
    let Some(store) = ctx.store() else {
        return;
    };
    // A pause with no parked effect is not a suspension this mechanism
    // produced; and a stale image from an earlier pause must never outlive the
    // state it described, so drop it rather than leave it on disk.
    let Some((op_id, effect, _)) = engine.vm.suspended_effects.first().cloned() else {
        let _ = store.delete_blob(IMAGE_BLOB);
        return;
    };
    let vm = match engine.vm.snapshot_image() {
        Ok(vm) => vm,
        Err(err) => {
            // Routine: live state with no serialized form (a queued Rust job, a
            // post-baseline native closure). The run resumes by replay.
            tracing::debug!(error = %err, "mainline pause not imageable; resume stays journal replay");
            let _ = store.delete_blob(IMAGE_BLOB);
            return;
        }
    };
    let pending_seq = ctx
        .active_pending_host_operation()
        .map(|pending| pending.seq)
        .unwrap_or_default();
    let envelope = MainlineImage {
        version: ENVELOPE_VERSION,
        entry_key: entry_key.to_string(),
        entry_hash: entry_hash(entry_src),
        call_log_len: ctx.call_log_len(),
        effect,
        op_id,
        pending_seq,
        vm,
    };
    match serde_json::to_vec(&envelope) {
        Ok(bytes) => {
            if let Err(err) = store.put_blob(IMAGE_BLOB, &bytes) {
                tracing::debug!(error = %err, "storing mainline VM image");
            }
        }
        Err(err) => tracing::debug!(error = %err, "encoding mainline VM image"),
    }
}

/// Decide whether this resume can restore from a stored image instead of
/// re-executing. Every `None` is a silent, logged decline back onto the replay
/// path — which is always correct.
pub(crate) fn accept(
    ctx: &RuntimeContext,
    entry_key: &str,
    entry_src: &str,
) -> Option<AcceptedImage> {
    let bytes = ctx.store()?.get_blob(IMAGE_BLOB).ok().flatten()?;
    let envelope: MainlineImage = match serde_json::from_slice(&bytes) {
        Ok(envelope) => envelope,
        Err(err) => {
            tracing::debug!(error = %err, "stored mainline VM image did not decode; resuming by replay");
            return None;
        }
    };
    let decline = |why: &str| {
        tracing::debug!(reason = %why, "mainline VM image declined; resuming by replay");
        None::<AcceptedImage>
    };
    if envelope.version != ENVELOPE_VERSION {
        return decline("image envelope version differs from this build");
    }
    if envelope.entry_key != entry_key {
        return decline("image was taken against a different entry module");
    }
    if envelope.entry_hash != entry_hash(entry_src) {
        return decline("the entry source changed since the image was taken");
    }
    // The image is the program's state after exactly `call_log_len` recorded
    // calls; the only journal it can continue is that prefix plus the single
    // delivered record the resume path injects at the pending seq. Anything
    // else (a shorter `--until-seq` log, an undelivered CLI resume, a second
    // delivery) has to re-execute.
    let records = ctx.replay_records()?;
    if records.len() != envelope.call_log_len + 1 {
        return decline("the resume journal is not this image's frontier plus one delivery");
    }
    let delivered: &CallRecord = records.last()?;
    if delivered.function != envelope.effect || delivered.seq != envelope.pending_seq {
        return decline("the delivered record does not answer the parked effect");
    }
    if delivered.error.is_some() {
        return decline("the delivered record carries an error");
    }
    Some(AcceptedImage {
        image: envelope.vm,
        op_id: envelope.op_id,
        delivered: delivered.result.clone(),
    })
}

/// Drop any stored image for this run. Called once a run settles: the program
/// it described no longer exists, and leaving it costs storage for nothing.
pub(crate) fn clear(ctx: &RuntimeContext) -> Result<()> {
    if let Some(store) = ctx.store() {
        store.delete_blob(IMAGE_BLOB)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderRegistry;
    use crate::runtime::engine::{Engine, RunResult};
    use crate::runtime::template::TemplateEngine;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex as StdMutex};

    /// `CHIDORI_MAINLINE_IMAGE` is process-global, so the tests that set it run
    /// one at a time — a concurrent test would otherwise observe a flag it did
    /// not ask for, which is exactly what the flag-off case asserts about.
    static FLAG_LOCK: StdMutex<()> = StdMutex::new(());

    /// Restores `CHIDORI_MAINLINE_IMAGE` even if the test panics.
    struct FlagGuard;

    impl FlagGuard {
        fn on() -> FlagGuard {
            std::env::set_var("CHIDORI_MAINLINE_IMAGE", "1");
            FlagGuard
        }
    }

    impl Drop for FlagGuard {
        fn drop(&mut self) {
            std::env::remove_var("CHIDORI_MAINLINE_IMAGE");
        }
    }

    /// An agent that journals on both sides of its `input()` pause, so the
    /// differential below compares a history with records before AND after the
    /// resume point — the part a re-execution replays and the part it runs live.
    const AGENT: &str = r#"
        export async function agent(input, chidori) {
            await chidori.log("before");
            const answer = await chidori.input("Approve?");
            await chidori.log("after");
            return { answer, seen: input.seen };
        }
    "#;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chidori-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn engine(dir: &Path, base: &Path) -> Engine {
        Engine::new(
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(dir)),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
        )
        .with_persist_base(base.to_path_buf())
    }

    /// The synthetic `input` record the resume path injects at the pending seq.
    fn delivery(seq: u64, prompt: &str, answer: &str) -> CallRecord {
        CallRecord {
            seq,
            parent_seq: None,
            function: "input".to_string(),
            args: serde_json::json!({ "prompt": prompt }),
            result: Value::String(answer.to_string()),
            duration_ms: 0,
            token_usage: None,
            timestamp: chrono::Utc::now(),
            error: None,
        }
    }

    type RecordShape = (u64, Option<u64>, String, Value, Value, Option<String>);

    /// Journal identity, ignoring the fields no two runs can share (timings).
    fn shape(records: &[CallRecord]) -> Vec<RecordShape> {
        records
            .iter()
            .map(|r| {
                (
                    r.seq,
                    r.parent_seq,
                    r.function.clone(),
                    r.args.clone(),
                    r.result.clone(),
                    r.error.clone(),
                )
            })
            .collect()
    }

    /// Pause, optionally tamper with what the pause persisted, then resume with
    /// the delivered answer.
    fn pause_then_resume(
        dir: &Path,
        base: &Path,
        mutate_image: Option<&dyn Fn(&Path)>,
    ) -> (RunResult, RunResult) {
        let path = dir.join("agent.ts");
        std::fs::write(&path, AGENT).unwrap();
        let input = serde_json::json!({ "seen": 7 });

        let paused = engine(dir, base).run_pausable(&path, &input).unwrap();
        let pending = paused.paused.clone().expect("the agent pauses on input()");
        if let Some(mutate) = mutate_image {
            mutate(&base.join(&paused.run_id));
        }

        let mut replay = paused.call_log.clone().into_records();
        replay.push(delivery(pending.seq, &pending.prompt, "yes"));
        let resumed = engine(dir, base)
            .run_replay_pausable_with_host_promises_and_vfs_preserving_run_id(
                &path,
                &input,
                replay,
                Vec::new(),
                crate::runtime::vfs::Vfs::new(),
                paused.run_id.clone(),
            )
            .unwrap();
        (paused, resumed)
    }

    /// A restored VM must be imageable again, or the mechanism would work
    /// exactly once per run. Two pauses, each resumed from the image the
    /// previous leg wrote, must land the same output and history as two plain
    /// re-execution resumes.
    #[test]
    fn a_restored_vm_images_again_at_its_next_pause() {
        let _flag = FLAG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let two_pauses = r#"
            export async function agent(input, chidori) {
                const first = await chidori.input("One?");
                await chidori.log(first);
                const second = await chidori.input("Two?");
                return { first, second };
            }
        "#;

        // `imaged` runs with the flag on for both legs; `plain` is the
        // re-execution control.
        let run = |flag_on: bool, base: &Path, dir: &Path| -> (RunResult, usize) {
            let _on = flag_on.then(FlagGuard::on);
            let path = dir.join("agent.ts");
            std::fs::write(&path, two_pauses).unwrap();
            let input = serde_json::json!({});

            let first = engine(dir, base).run_pausable(&path, &input).unwrap();
            let first_pending = first.paused.clone().expect("first pause");
            let mut log = first.call_log.clone().into_records();
            log.push(delivery(first_pending.seq, &first_pending.prompt, "a"));

            let resume_with = |log: Vec<CallRecord>| {
                engine(dir, base)
                    .run_replay_pausable_with_host_promises_and_vfs_preserving_run_id(
                        &path,
                        &input,
                        log,
                        Vec::new(),
                        crate::runtime::vfs::Vfs::new(),
                        first.run_id.clone(),
                    )
                    .unwrap()
            };

            let second = resume_with(log);
            let second_pending = second.paused.clone().expect("second pause");
            let images_written = usize::from(base.join(&first.run_id).join(IMAGE_BLOB).exists());
            let mut log = second.call_log.clone().into_records();
            log.push(delivery(second_pending.seq, &second_pending.prompt, "b"));
            (resume_with(log), images_written)
        };

        let dir = scratch("mainline-image-two-pauses");
        let (imaged, imaged_images) = run(true, &dir.join("imaged"), &dir);
        let (plain, plain_images) = run(false, &dir.join("plain"), &dir);

        assert_eq!(imaged_images, 1, "the second pause wrote an image too");
        assert_eq!(plain_images, 0);
        assert_eq!(
            imaged.output,
            serde_json::json!({ "first": "a", "second": "b" })
        );
        assert_eq!(imaged.output, plain.output);
        assert_eq!(
            shape(imaged.call_log.records()),
            shape(plain.call_log.records())
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// The differential the whole mechanism rests on: a resume that restores the
    /// VM image and one that re-executes the journal must produce the same
    /// output AND the same durable history. The image is a cache over
    /// deterministic computation; if the two ever disagreed it would not be one.
    #[test]
    fn image_resume_and_replay_resume_agree_on_output_and_journal() {
        let _flag = FLAG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("mainline-image-differential");
        let imaged_base = dir.join("imaged");
        let replayed_base = dir.join("replayed");

        let (imaged_pause, imaged_resume) = {
            let _on = FlagGuard::on();
            pause_then_resume(&dir, &imaged_base, None)
        };
        let imaged_blob = imaged_base.join(&imaged_pause.run_id).join(IMAGE_BLOB);
        // The settled resume cleared the image it restored from — the program
        // it described is gone — so its existence is asserted through the
        // history the restore produced, below.
        assert!(!imaged_blob.exists());

        let (replayed_pause, replayed_resume) = pause_then_resume(&dir, &replayed_base, None);
        assert!(!replayed_base
            .join(&replayed_pause.run_id)
            .join(IMAGE_BLOB)
            .exists());

        assert_eq!(
            imaged_resume.output,
            serde_json::json!({ "answer": "yes", "seen": 7 })
        );
        assert_eq!(imaged_resume.output, replayed_resume.output);
        assert_eq!(
            shape(imaged_pause.call_log.records()),
            shape(replayed_pause.call_log.records()),
            "the paused artifacts must be identical — a suspension records no \
             more than an unwind"
        );
        assert_eq!(
            shape(imaged_resume.call_log.records()),
            shape(replayed_resume.call_log.records()),
            "restoring an image and re-executing the journal must leave the \
             same history"
        );
        // The restore skipped re-execution but still owns the whole history.
        assert_eq!(imaged_resume.call_log.records().len(), 3);
        assert_eq!(imaged_resume.replayed_calls, 2);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// The pause itself writes the image beside — never instead of — the
    /// journal scaffold the unwinding path has always written.
    #[test]
    fn pause_writes_an_image_beside_the_paused_artifact() {
        let _flag = FLAG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _on = FlagGuard::on();
        let dir = scratch("mainline-image-artifact");
        let base = dir.join("runs");
        let path = dir.join("agent.ts");
        std::fs::write(&path, AGENT).unwrap();

        let paused = engine(&dir, &base)
            .run_pausable(&path, &serde_json::json!({ "seen": 7 }))
            .unwrap();
        assert!(paused.paused.is_some());
        let run_dir = base.join(&paused.run_id);
        assert!(
            run_dir.join(IMAGE_BLOB).exists(),
            "the pause wrote an image"
        );
        assert!(
            run_dir.join("records.jsonl").exists(),
            "the classic paused journal is untouched"
        );
        assert!(run_dir
            .join(crate::runtime::snapshot::SNAPSHOT_MANIFEST_FILE)
            .exists());
        assert!(run_dir
            .join(crate::runtime::snapshot::PENDING_HOST_OPERATION_FILE)
            .exists());

        let _ = std::fs::remove_dir_all(dir);
    }

    /// A corrupt image is a latency problem, never a correctness one: the
    /// journal is the source of truth, so every failure mode falls back to
    /// re-execution and still finishes.
    #[test]
    fn corrupt_image_falls_back_to_replay() {
        let _flag = FLAG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _on = FlagGuard::on();

        // Undecodable bytes: declined before the engine is even built.
        let dir = scratch("mainline-image-garbage");
        let base = dir.join("runs");
        let (_, resumed) = pause_then_resume(
            &dir,
            &base,
            Some(&|run_dir: &Path| {
                std::fs::write(run_dir.join(IMAGE_BLOB), b"{not json").unwrap();
            }),
        );
        assert_eq!(
            resumed.output,
            serde_json::json!({ "answer": "yes", "seen": 7 })
        );
        let _ = std::fs::remove_dir_all(dir);

        // A well-formed envelope whose VM image does not belong to this
        // baseline: accepted by the envelope checks, refused by `restore_image`,
        // and the run re-executes on a fresh engine.
        let dir = scratch("mainline-image-mismatch");
        let base = dir.join("runs");
        let (_, resumed) = pause_then_resume(
            &dir,
            &base,
            Some(&|run_dir: &Path| {
                let blob = run_dir.join(IMAGE_BLOB);
                let mut envelope: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&blob).unwrap()).unwrap();
                envelope["vm"]["baseline_digest"] = serde_json::json!(1_u64);
                std::fs::write(&blob, serde_json::to_vec(&envelope).unwrap()).unwrap();
            }),
        );
        assert_eq!(
            resumed.output,
            serde_json::json!({ "answer": "yes", "seen": 7 })
        );
        assert_eq!(resumed.call_log.records().len(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Flag off is the default, and the default must be the old path exactly:
    /// no image written, no image read, and the pause still unwinds.
    #[test]
    fn flag_off_writes_no_image_and_resumes_by_replay() {
        let _flag = FLAG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("CHIDORI_MAINLINE_IMAGE");
        let dir = scratch("mainline-image-off");
        let base = dir.join("runs");
        let (paused, resumed) = pause_then_resume(&dir, &base, None);
        assert!(!base.join(&paused.run_id).join(IMAGE_BLOB).exists());
        assert_eq!(
            resumed.output,
            serde_json::json!({ "answer": "yes", "seen": 7 })
        );
        assert_eq!(resumed.replayed_calls, 2);
        let _ = std::fs::remove_dir_all(dir);
    }
}
