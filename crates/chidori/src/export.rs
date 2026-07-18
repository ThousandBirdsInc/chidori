//! `chidori export` — carve a minimal, committable verification fixture out
//! of a full run directory.
//!
//! A run directory carries everything `chidori resume` might need — the
//! multi-megabyte runtime snapshot blob (`runtime.snapshot`), the host promise
//! table, pending-operation state, leases — which makes "commit a run and
//! `chidori verify` it in CI" impractical at tens of MB per checkpoint.
//! `chidori verify` itself reads only four artifacts:
//!
//! - `records.jsonl` / `checkpoint.json` — the call journal (the two are the
//!   same log twice; the export writes one compacted `records.jsonl` from
//!   their union),
//! - `runtime.snapshot.json` — the snapshot *manifest* (source fingerprints,
//!   policy/ABI, recorded default model) that gates source drift,
//! - `output.json` — the recorded output the replay must reproduce,
//! - `input.json` — the run's input.
//!
//! `cmd_export` copies exactly that set into `<dest>/<run_id>/`, refusing runs
//! whose journal is not yet a complete, verifiable record (live lease, pending
//! host operation, no recorded output). The fixture is consumed with
//! `chidori verify <agent.ts> <run_id> --runs-dir <dest>`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::runtime::snapshot::{
    SnapshotManifest, PENDING_HOST_OPERATION_FILE, SNAPSHOT_MANIFEST_FILE,
};
use crate::runtime::store::{FsRunStore, RunLease, RunStore as _, LEASE_FILE, RECORDS_FILE};

/// The recorded-output artifact `chidori verify` compares the replay against.
const OUTPUT_FILE: &str = "output.json";
/// The run-input artifact `chidori verify` replays with.
const INPUT_FILE: &str = "input.json";

pub fn cmd_export(run_id: &str, fixture_dest: &Path, dir: Option<&Path>) -> Result<()> {
    let base_dir = dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let run_dir = base_dir.join(".chidori").join("runs").join(run_id);
    if !run_dir.is_dir() {
        anyhow::bail!("No persisted run at {}", run_dir.display());
    }
    let store = FsRunStore::new(run_dir.clone());

    // --- Refuse runs whose journal is not a complete, verifiable record. ---

    // A live (unexpired) lease means some process still owns and is writing
    // this run: its journal is mid-flight, not a fixture.
    if let Some(bytes) = store.get_blob(LEASE_FILE)? {
        if let Ok(lease) = serde_json::from_slice::<RunLease>(&bytes) {
            if lease.expires_at > chrono::Utc::now() {
                anyhow::bail!(
                    "refusing to export run {run_id}: it is still leased by `{}` (until {}) — \
                     a running run's journal is incomplete. Wait for the run to finish (or the \
                     lease to expire) and re-export.",
                    lease.owner,
                    lease.expires_at
                );
            }
        }
    }

    // The snapshot manifest is what lets `verify` refuse source drift and
    // recover the recorded model; a fixture without one would verify against
    // whatever code happens to be on disk.
    let manifest_bytes = store.get_blob(SNAPSHOT_MANIFEST_FILE)?.ok_or_else(|| {
        anyhow::anyhow!(
            "refusing to export run {run_id}: no {SNAPSHOT_MANIFEST_FILE} under {} — without \
             the snapshot manifest, `chidori verify` cannot check the agent source against \
             the recorded fingerprints",
            run_dir.display()
        )
    })?;
    let manifest: SnapshotManifest = serde_json::from_slice(&manifest_bytes)
        .with_context(|| format!("parsing {}", run_dir.join(SNAPSHOT_MANIFEST_FILE).display()))?;

    // A pending host operation (or its durable artifact) marks a paused run;
    // `verify` only accepts runs that replay to completion.
    if manifest.pending.is_some() || store.get_blob(PENDING_HOST_OPERATION_FILE)?.is_some() {
        anyhow::bail!(
            "refusing to export run {run_id}: it is paused at a pending host operation — \
             only completed runs can be verified. Resume it to completion first."
        );
    }

    // `output.json` is written when a run settles; without it there is no
    // recorded output for `verify` to hold the replay to.
    let output_bytes = store.get_blob(OUTPUT_FILE)?.ok_or_else(|| {
        anyhow::anyhow!(
            "refusing to export run {run_id}: no recorded {OUTPUT_FILE} — the run never \
             completed (it is still running, paused, or failed), so its journal is not a \
             verifiable record. Only completed runs can be exported as fixtures."
        )
    })?;

    let records = store.load_call_log()?.ok_or_else(|| {
        anyhow::anyhow!(
            "refusing to export run {run_id}: no call journal (records.jsonl / \
             checkpoint.json) under {}",
            run_dir.display()
        )
    })?;

    // --- Write the fixture: `<dest>/<run_id>/` with exactly what verify reads. ---

    let fixture_dir = fixture_dest.join(run_id);
    std::fs::create_dir_all(&fixture_dir)
        .with_context(|| format!("creating {}", fixture_dir.display()))?;

    // One compacted journal from the checkpoint ∪ appended-tail union, so the
    // fixture needs no `checkpoint.json` duplicate of the same log.
    let mut journal = Vec::new();
    for record in &records {
        journal.extend(serde_json::to_vec(record)?);
        journal.push(b'\n');
    }
    let mut written: Vec<(String, u64)> = Vec::new();
    let mut write_artifact = |name: &str, bytes: &[u8]| -> Result<()> {
        let path = fixture_dir.join(name);
        std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
        written.push((name.to_string(), bytes.len() as u64));
        Ok(())
    };
    write_artifact(RECORDS_FILE, &journal)?;
    write_artifact(SNAPSHOT_MANIFEST_FILE, &manifest_bytes)?;
    write_artifact(OUTPUT_FILE, &output_bytes)?;
    if let Some(input_bytes) = store.get_blob(INPUT_FILE)? {
        write_artifact(INPUT_FILE, &input_bytes)?;
    }

    // --- Report: what was copied, what it saved, how to consume it. ---

    let run_dir_size = dir_size(&run_dir);
    let fixture_size: u64 = written.iter().map(|(_, size)| size).sum();
    let name_width = written
        .iter()
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(0);
    println!("exported fixture: {}", fixture_dir.display());
    for (name, size) in &written {
        println!("  {name:<name_width$}  {:>10}", human_size(*size));
    }
    println!(
        "exported fixture: {} (run dir was {})",
        human_size(fixture_size),
        human_size(run_dir_size)
    );
    println!(
        "verify with: chidori verify {} {run_id} --runs-dir {}",
        manifest.entry.path.display(),
        fixture_dest.display()
    );
    Ok(())
}

/// Total size in bytes of every regular file under `path`, recursively.
/// Unreadable entries count as zero — the number feeds a summary line, not a
/// correctness decision.
fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry_path.is_dir() {
                stack.push(entry_path);
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

/// `312 B` / `4.2 KB` / `24.1 MB` — coarse, human-first sizes for the summary.
fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let bytes_f = bytes as f64;
    if bytes_f >= MB {
        format!("{:.1} MB", bytes_f / MB)
    } else if bytes_f >= KB {
        format!("{:.1} KB", bytes_f / KB)
    } else {
        format!("{bytes} B")
    }
}
