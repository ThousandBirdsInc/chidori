//! Source history — a git-like record of the agent's *implementation*
//! alongside the run's *execution* journal.
//!
//! The effect journal (`records.jsonl` / `checkpoint.json`) is a complete
//! history of what the run **did**; this module keeps the matching history of
//! what the run **was** — every version of the agent's source (entry + every
//! imported module) that existed alongside the execution history. Without it,
//! the code side of a run's history is lossy: the manifest keeps only the
//! *latest* fingerprints, edit-and-resume overwrites the previous identity
//! with a warning, and `branch-rerun` edits `source.ts` in place — the version
//! that produced the recorded prefix is gone.
//!
//! The model is deliberately git-shaped, one store per run (or per branch
//! sub-run), flowing through the same [`RunStore`] handle as every other run
//! artifact:
//!
//! ```text
//! <run dir>/history/
//!   objects/<sha256 hex>     content-addressed source blobs (full file text,
//!                            stored once per unique content)
//!   commits.jsonl            append-only commit log, one JSON commit per line
//! <run dir>/branches/op-*/branch-*/history/   the same shape per branch
//! ```
//!
//! A [`SourceCommit`] snapshots the module tree (path → object id) at the
//! moment a code version became active, with parent commit ids forming the
//! DAG: a run's trunk is a linear chain (`run_start` →
//! `resume_source_change` → …), and each branch store's first commit
//! (`branch_fork`) points back at the parent run's head commit — so the fork
//! points and per-branch edit chains are first-class history, exactly like
//! branches in git.
//!
//! The link *back* to execution history is `journal_frontier`: the number of
//! journaled call records that already existed when this code version took
//! over. Between two consecutive commits, every record with
//! `frontier_a < record_index <= frontier_b` executed under commit `a`'s
//! code — which is what lets `chidori history` render the interleaved
//! timeline ("seq 1..42 ran under a1b2c3d, then the edit d4e5f6a took over").
//!
//! Recording points (all best-effort — a history write failure warns, it
//! never fails the run):
//! - **run start** — [`crate::runtime::engine::ScaffoldPersister`] records the
//!   initial tree on the run's first persist;
//! - **edit-and-resume** — `validate_manifest_for_resume` records the accepted
//!   change (and synthesizes a `run_start` commit from the persisted
//!   `DurableBlob.bundle` for runs recorded before source history existed);
//! - **branch fork** — `host_branch` records each variant's source in its
//!   branch store, parented on the run's head commit;
//! - **branch resume / edit-and-rerun** — `host_branch` records the branch's
//!   current `source.ts` before running it.
//!
//! Every recording point dedupes against the store's head commit: recording
//! an identical tree is a no-op, so resume-without-edit, replay, and repeated
//! safepoints never grow the history.
//!
//! The read side is [`materialize_source`]: because the store holds every
//! module's full text, a node that does NOT have an agent's source tree on
//! disk can rebuild it from the head commit and run from there — which is
//! what lets any node in a fleet wake any run it can lease
//! (`docs/detached-agents.md`).

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::runtime::store::RunStore;

/// Run-dir-relative key of the append-only commit log.
pub const SOURCE_COMMITS_FILE: &str = "history/commits.jsonl";
/// Run-dir-relative key prefix of the content-addressed source blobs.
pub const SOURCE_OBJECTS_PREFIX: &str = "history/objects/";
/// Head-commit cache (`history/HEAD.json`): the newest commit, duplicated so
/// the recording hot path reads one small blob instead of parsing the whole
/// log. The log stays authoritative — every reader of the *chain*
/// ([`load_commits`]) ignores HEAD, and [`head_commit`] falls back to the
/// log when HEAD is missing or unreadable. A crash between the log append
/// and the HEAD rewrite leaves HEAD one commit stale; the worst consequence
/// is one redundant (identical-to-parent-tree, "no file changes") commit on
/// the next recording — never lost history.
pub const SOURCE_HEAD_FILE: &str = "history/HEAD.json";
const SOURCE_COMMIT_VERSION: u32 = 1;
/// The cross-run object cache's directory name, a sibling of the standard
/// `runs/` base (i.e. `.chidori/history-objects/`). See [`cross_run_cache`].
pub const CROSS_RUN_OBJECT_CACHE_DIR: &str = "history-objects";

/// Why a source version was recorded — the git-log "subject" of the commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceCommitEvent {
    /// The tree the run started (or first persisted) with.
    RunStart,
    /// An edit accepted through the `--allow-source-change` /
    /// `"allow_source_change": true` opt-in on a resume surface.
    ResumeSourceChange,
    /// A `chidori.branch` variant's own source at fork time. The commit's
    /// parent is the parent run's head commit — the git-like fork point.
    BranchFork,
    /// The branch's `source.ts` as it stood when a paused branch was resumed
    /// (recorded only if it changed since the previous branch commit).
    BranchResume,
    /// The branch's edited `source.ts` at `chidori branch-rerun` time.
    BranchRerun,
}

impl fmt::Display for SourceCommitEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SourceCommitEvent::RunStart => "run_start",
            SourceCommitEvent::ResumeSourceChange => "resume_source_change",
            SourceCommitEvent::BranchFork => "branch_fork",
            SourceCommitEvent::BranchResume => "branch_resume",
            SourceCommitEvent::BranchRerun => "branch_rerun",
        })
    }
}

/// One file in a commit's snapshot: its path and the content-addressed id of
/// its full text (`sha256:<hex>`, resolvable via [`load_object`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceTreeEntry {
    pub path: PathBuf,
    pub object: String,
}

/// One recorded version of the agent's implementation, linked into the
/// git-like DAG (`parents`) and anchored to the execution journal
/// (`journal_frontier`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceCommit {
    /// `sha256:<hex>` over the commit's identity (parents + event + tree +
    /// anchor) — deterministic, so identical recordings collide into one id.
    pub id: String,
    #[serde(default)]
    pub version: u32,
    /// Previous commit id(s). The store's own head first; a branch-fork
    /// commit carries the parent *run's* head as its fork parent.
    pub parents: Vec<String>,
    pub event: SourceCommitEvent,
    /// The run this version belongs to (the parent run id for branch stores).
    pub run_id: String,
    /// The branch sub-run this version belongs to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<String>,
    /// The entry module's path within `tree`.
    pub entry_path: PathBuf,
    /// The full module tree at this version, sorted by path.
    pub tree: Vec<SourceTreeEntry>,
    /// How many journaled call records already existed when this version
    /// became active: records after this frontier (up to the next commit's)
    /// executed under this code.
    pub journal_frontier: u64,
    pub created_at: DateTime<Utc>,
}

impl SourceCommit {
    pub fn tree_object(&self, path: &Path) -> Option<&str> {
        self.tree
            .iter()
            .find(|entry| entry.path == path)
            .map(|entry| entry.object.as_str())
    }
}

/// Abbreviate a `sha256:<hex>` id to its first 12 hex chars for display.
pub fn short_id(id: &str) -> &str {
    let hex = id.strip_prefix("sha256:").unwrap_or(id);
    &hex[..hex.len().min(12)]
}

/// Whether `id` matches a user-supplied commit reference: the full id, the
/// full hex, or any hex prefix of at least 4 chars.
pub fn id_matches(id: &str, reference: &str) -> bool {
    let hex = id.strip_prefix("sha256:").unwrap_or(id);
    let wanted = reference.strip_prefix("sha256:").unwrap_or(reference);
    wanted.len() >= 4 && hex.starts_with(wanted)
}

/// Content-address a source text: `sha256:<hex>` over its bytes.
pub fn object_id(text: &str) -> String {
    format!("sha256:{}", object_hex(text))
}

/// The bare hex of a text's content address — its file name inside an
/// objects directory (`history/objects/<hex>`, or the cross-run cache).
pub fn object_hex(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn object_key(id: &str) -> Result<String> {
    let hex = id
        .strip_prefix("sha256:")
        .filter(|hex| !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| anyhow::anyhow!("invalid source object id `{id}`"))?;
    Ok(format!("{SOURCE_OBJECTS_PREFIX}{hex}"))
}

/// The input to [`record_commit`]: the event, identity, and the full source
/// tree (paths + texts) of the version being recorded.
pub struct CommitInput<'a> {
    pub event: SourceCommitEvent,
    pub run_id: &'a str,
    pub branch_id: Option<&'a str>,
    pub entry_path: &'a Path,
    /// Every file in the version: `(path, full text)`. The entry must be
    /// among them; imported modules follow.
    pub files: &'a [(PathBuf, String)],
    pub journal_frontier: u64,
    /// An extra parent from *outside* this store's chain — the parent run's
    /// head commit id for a `branch_fork`.
    pub extra_parent: Option<String>,
    /// Content-addressed directories (flat `<sha256 hex>` files) that may
    /// already hold this version's objects — the parent run's
    /// `history/objects/`, sibling branches', the cross-run cache. A hit is
    /// hardlinked (falling back to a copy-on-write clone) into this store
    /// instead of rewritten, so shared content is stored once per machine.
    /// Only applies when the store is a local directory
    /// ([`RunStore::blob_os_path`]); harmless otherwise.
    pub share_from: &'a [PathBuf],
    /// The machine-local cross-run cache ([`cross_run_cache`]): newly
    /// recorded objects are also linked (or written) into it, best-effort,
    /// so the *next* run of the same agent dedupes its whole tree against
    /// one shared copy instead of storing another.
    pub backfill_cache: Option<&'a Path>,
}

/// The cross-run object cache for a run base directory: `history-objects/`
/// as a **sibling** of the standard `runs/` base — `.chidori/history-objects`
/// next to `.chidori/runs` — so run-id enumeration over the base never sees
/// it. `None` for a non-standard base (embedders, tests pointing at
/// arbitrary directories), which keeps the cache from being planted in
/// arbitrary parent directories.
pub fn cross_run_cache(run_base: &Path) -> Option<PathBuf> {
    if run_base.file_name() == Some(std::ffi::OsStr::new("runs")) {
        run_base
            .parent()
            .map(|parent| parent.join(CROSS_RUN_OBJECT_CACHE_DIR))
    } else {
        None
    }
}

/// Materialize `dst` with the same content as the existing file `src`,
/// sharing storage where the platform allows: hardlink first (true
/// deduplication on any filesystem — safe here because history objects are
/// content-addressed and never mutated in place), then [`std::fs::copy`],
/// which clones copy-on-write on filesystems that support it (btrfs/XFS via
/// `copy_file_range`, APFS via `clonefile`) and degrades to a plain copy
/// elsewhere. Fails if `dst` already exists (callers check first).
fn link_or_copy(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::hard_link(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => std::fs::copy(src, dst).map(|_| ()),
    }
}

/// Copy-on-write clone for **mutable** destinations (a branch's editable
/// `source.ts`): never hardlinks — an edit through a hardlink would rewrite
/// the shared immutable object — but `std::fs::copy` shares blocks (reflink)
/// on CoW filesystems until the file is edited, and is an ordinary copy
/// elsewhere.
pub(crate) fn cow_clone(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::copy(src, dst).map(|_| ())
}

fn write_file_creating_dirs(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)
}

/// Record a source version into the store's history: write any new
/// content-addressed objects and append one commit. Returns `Ok(None)`
/// without writing anything when the tree is identical to the store's head
/// commit — recording is idempotent, so callers invoke it unconditionally at
/// their integration point and let the dedupe decide.
///
/// Cost model: the head check reads the small `HEAD.json` cache (not the
/// log), the commit lands as one O(1) line append, and each object is one
/// existence stat plus — only when new — a hardlink/CoW-clone from
/// `share_from`/the cache, or a write.
pub fn record_commit(store: &dyn RunStore, input: CommitInput<'_>) -> Result<Option<SourceCommit>> {
    // Sorted, deduped-by-path tree: deterministic identity regardless of the
    // caller's file ordering.
    let mut by_path: BTreeMap<&Path, &str> = BTreeMap::new();
    for (path, text) in input.files {
        by_path.entry(path.as_path()).or_insert(text.as_str());
    }
    anyhow::ensure!(
        by_path.contains_key(input.entry_path),
        "source commit entry {} is not among its files",
        input.entry_path.display()
    );
    let tree: Vec<SourceTreeEntry> = by_path
        .iter()
        .map(|(path, text)| SourceTreeEntry {
            path: path.to_path_buf(),
            object: object_id(text),
        })
        .collect();

    let head = head_commit(store)?;
    let head = head.as_ref();
    if let Some(head) = head {
        if head.tree == tree {
            return Ok(None);
        }
    }
    let mut parents: Vec<String> = head.map(|head| head.id.clone()).into_iter().collect();
    if let Some(extra) = input.extra_parent {
        if !parents.contains(&extra) {
            parents.push(extra);
        }
    }

    // Deterministic identity: everything that *is* the version, nothing that
    // merely dates it — so the same recording always minting the same id is a
    // testable invariant and an accidental double-append is detectable.
    let mut identity = String::from("chidori-source-commit-v1\n");
    for parent in &parents {
        identity.push_str("parent ");
        identity.push_str(parent);
        identity.push('\n');
    }
    identity.push_str(&format!(
        "event {}\nrun {}\nbranch {}\nentry {}\nfrontier {}\n",
        input.event,
        input.run_id,
        input.branch_id.unwrap_or("-"),
        input.entry_path.display(),
        input.journal_frontier
    ));
    for entry in &tree {
        identity.push_str(&format!("tree {} {}\n", entry.object, entry.path.display()));
    }
    let id = format!("sha256:{:x}", Sha256::digest(identity.as_bytes()));

    let commit = SourceCommit {
        id,
        version: SOURCE_COMMIT_VERSION,
        parents,
        event: input.event,
        run_id: input.run_id.to_string(),
        branch_id: input.branch_id.map(ToOwned::to_owned),
        entry_path: input.entry_path.to_path_buf(),
        tree,
        journal_frontier: input.journal_frontier,
        created_at: Utc::now(),
    };

    // Objects before the commit that references them: a crash in between
    // leaves orphaned (harmless, content-addressed) blobs, never a commit
    // pointing at missing text.
    for (path, text) in input.files {
        let entry_object = object_id(text);
        let hex = entry_object
            .strip_prefix("sha256:")
            .expect("object_id always returns a sha256: id");
        let key = object_key(&entry_object)?;
        if store.has_blob(&key)? {
            continue;
        }
        // Dedupe before write: an identical object anywhere in the shared
        // set (parent run, sibling branch, cross-run cache) is hardlinked or
        // CoW-cloned in — one stored copy per machine for unchanged content.
        let dst = store.blob_os_path(&key);
        let mut materialized = false;
        if let Some(dst) = &dst {
            for source_dir in input
                .share_from
                .iter()
                .map(PathBuf::as_path)
                .chain(input.backfill_cache)
            {
                let candidate = source_dir.join(hex);
                if candidate.is_file() && link_or_copy(&candidate, dst).is_ok() {
                    materialized = true;
                    break;
                }
            }
        }
        if !materialized {
            store
                .put_blob(&key, text.as_bytes())
                .with_context(|| format!("writing source object for {}", path.display()))?;
        }
        // Back-fill the cross-run cache so the next run links instead of
        // writing. Best-effort: cache misses never fail a recording.
        if let Some(cache) = input.backfill_cache {
            let cache_path = cache.join(hex);
            if !cache_path.is_file() {
                let cached = match &dst {
                    Some(dst) => link_or_copy(dst, &cache_path)
                        .or_else(|_| write_file_creating_dirs(&cache_path, text.as_bytes())),
                    None => write_file_creating_dirs(&cache_path, text.as_bytes()),
                };
                if let Err(err) = cached {
                    tracing::debug!(
                        "source history: could not back-fill object cache {}: {err}",
                        cache_path.display()
                    );
                }
            }
        }
    }
    // O(1) append, then refresh the head cache (log first: the log is
    // authoritative, HEAD is a rebuildable cache of its last line).
    store
        .append_blob_line(SOURCE_COMMITS_FILE, &serde_json::to_vec(&commit)?)
        .with_context(|| format!("appending {SOURCE_COMMITS_FILE}"))?;
    store
        .put_blob(SOURCE_HEAD_FILE, &serde_json::to_vec_pretty(&commit)?)
        .with_context(|| format!("writing {SOURCE_HEAD_FILE}"))?;
    Ok(Some(commit))
}

/// Load a store's commit chain, oldest first. A run with no recorded history
/// (persisted before this feature existed) loads as empty; a crash-truncated
/// trailing line is tolerated and skipped.
pub fn load_commits(store: &dyn RunStore) -> Result<Vec<SourceCommit>> {
    let Some(bytes) = store.get_blob(SOURCE_COMMITS_FILE)? else {
        return Ok(Vec::new());
    };
    let mut commits = Vec::new();
    for line in bytes.split(|b| *b == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<SourceCommit>(line) {
            Ok(commit) => commits.push(commit),
            Err(err) => {
                tracing::warn!("skipping unreadable source-history commit line: {err}");
            }
        }
    }
    Ok(commits)
}

/// The store's newest commit, if any: one read of the small `HEAD.json`
/// cache, falling back to scanning the authoritative log when HEAD is
/// missing (histories recorded before HEAD existed) or unreadable.
pub fn head_commit(store: &dyn RunStore) -> Result<Option<SourceCommit>> {
    if let Some(bytes) = store.get_blob(SOURCE_HEAD_FILE)? {
        if let Ok(commit) = serde_json::from_slice::<SourceCommit>(&bytes) {
            return Ok(Some(commit));
        }
        tracing::warn!("unreadable {SOURCE_HEAD_FILE}; rebuilding from the commit log");
    }
    Ok(load_commits(store)?.pop())
}

/// Load the full text behind a `sha256:<hex>` object id.
pub fn load_object(store: &dyn RunStore, id: &str) -> Result<Option<String>> {
    let Some(bytes) = store.get_blob(&object_key(id)?)? else {
        return Ok(None);
    };
    String::from_utf8(bytes)
        .map(Some)
        .context("source object is not UTF-8")
}

/// Read a set of module files (paths + texts) from disk — the shared shape
/// every recording point feeds into [`CommitInput::files`].
pub fn read_source_files(paths: &[PathBuf]) -> Result<Vec<(PathBuf, String)>> {
    paths
        .iter()
        .map(|path| {
            std::fs::read_to_string(path)
                .map(|text| (path.clone(), text))
                .with_context(|| format!("reading source file {}", path.display()))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Materialization — reading a run's implementation back out of the store
// ---------------------------------------------------------------------------

/// Directory holding source trees materialized from the durable store:
/// `<.chidori>/materialized/<run_id>/`. A **sibling** of the standard `runs/`
/// base (like the object cache), so run-id enumeration over the base never
/// sees it.
pub const MATERIALIZED_DIR: &str = "materialized";

/// The materialization root for one run under `run_base`. The run id reaches
/// here from a durable descriptor, so it is untrusted input to a path join:
/// only a safe single component is accepted.
pub fn materialization_root(run_base: &Path, run_id: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        safe_component(run_id),
        "run id `{run_id}` is not a safe path component for source materialization"
    );
    let base = if run_base.file_name() == Some(std::ffi::OsStr::new("runs")) {
        run_base.parent().unwrap_or(run_base).join(MATERIALIZED_DIR)
    } else {
        run_base.join(MATERIALIZED_DIR)
    };
    Ok(base.join(run_id))
}

/// The `valid_component` rule the cell store applies to ids that become path
/// segments: no empty/`.`/`..`, no separators, no surprises.
fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'))
}

/// Write the run's recorded implementation under `root` and return the entry
/// module's path there — how a node that does not have an agent's source tree
/// on local disk wakes it anyway (`docs/detached-agents.md` §Source
/// materialization).
///
/// The head commit is the whole tree (entry + every imported module). Runs
/// recorded before source history existed fall back to the snapshot
/// manifest's `DurableBlob.bundle`, which holds the entry text alone — enough
/// to wake a single-module agent, not enough for one with imports.
/// `Ok(None)` when the store holds neither.
///
/// Recorded paths are the *recording* node's paths (usually absolute), so the
/// tree is rebased onto `root` at its common ancestor: relative structure —
/// and therefore relative imports — survive the move. Nothing from the store
/// may escape `root`; see [`rebase_tree`].
///
/// Idempotent and cheap on the second call: a file already present with the
/// recorded content is left alone, so a re-wake re-reads at most the head
/// commit.
pub fn materialize_source(store: &dyn RunStore, root: &Path) -> Result<Option<PathBuf>> {
    let Some(commit) = head_commit(store)? else {
        return materialize_from_bundle(store, root);
    };
    let rebased = rebase_tree(commit.tree.iter().map(|entry| entry.path.as_path()))?;
    let mut entry_path = None;
    for (entry, relative) in commit.tree.iter().zip(&rebased) {
        let text = load_object(store, &entry.object)?.with_context(|| {
            format!(
                "source object {} for {} is missing from the run store",
                short_id(&entry.object),
                entry.path.display()
            )
        })?;
        let destination = root.join(relative);
        materialize_file(&destination, &text)?;
        if entry.path == commit.entry_path {
            entry_path = Some(destination);
        }
    }
    let entry_path = entry_path.with_context(|| {
        format!(
            "source commit {} does not carry its entry module {}",
            short_id(&commit.id),
            commit.entry_path.display()
        )
    })?;
    Ok(Some(entry_path))
}

/// Pre-source-history fallback: the entry text persisted in the snapshot
/// blob's `DurableBlob.bundle`. `Ok(None)` when the run has no manifest, no
/// blob, or an empty bundle.
fn materialize_from_bundle(store: &dyn RunStore, root: &Path) -> Result<Option<PathBuf>> {
    use crate::runtime::snapshot::{SnapshotManifest, SNAPSHOT_MANIFEST_FILE};

    let Some(bytes) = store.get_blob(SNAPSHOT_MANIFEST_FILE)? else {
        return Ok(None);
    };
    let manifest: SnapshotManifest =
        serde_json::from_slice(&bytes).context("parsing the run manifest to materialize source")?;
    let Some(bytes) = store.get_blob(&manifest.snapshot_file)? else {
        return Ok(None);
    };
    let Ok(blob) = serde_json::from_slice::<chidori_js::replay::DurableBlob>(&bytes) else {
        return Ok(None);
    };
    if blob.bundle.is_empty() {
        return Ok(None);
    }
    let relative = rebase_tree([manifest.entry.path.as_path()])?
        .pop()
        .expect("rebase_tree preserves its input length");
    let destination = root.join(relative);
    materialize_file(&destination, &blob.bundle)?;
    Ok(Some(destination))
}

/// Rebase recorded module paths onto a fresh root: strip the common ancestor
/// of the absolute ones (relative paths are already their own layout) so the
/// tree's shape is preserved rather than its recording machine's directories.
///
/// Security boundary: the paths come from the durable store, which the waking
/// node did not write. A rebased path that is not a chain of plain names —
/// `..`, a root, a drive prefix — is refused rather than written, so a
/// hostile or corrupt commit cannot place a file outside the materialization
/// root. Two modules that would collide onto one path are refused for the
/// same reason: the tree must map onto disk exactly as recorded.
fn rebase_tree<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Result<Vec<PathBuf>> {
    use std::path::Component;

    let paths: Vec<&Path> = paths.into_iter().collect();
    anyhow::ensure!(!paths.is_empty(), "recorded source tree is empty");
    for path in &paths {
        anyhow::ensure!(
            !path
                .components()
                .any(|component| matches!(component, Component::ParentDir)),
            "recorded source path `{}` escapes its tree",
            path.display()
        );
    }
    let ancestor = common_ancestor(
        paths
            .iter()
            .filter(|path| path.is_absolute())
            .filter_map(|path| path.parent()),
    );
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::with_capacity(paths.len());
    for path in &paths {
        let relative = match (&ancestor, path.is_absolute()) {
            (Some(ancestor), true) => path.strip_prefix(ancestor).unwrap_or(path).to_path_buf(),
            _ => path.to_path_buf(),
        };
        anyhow::ensure!(
            relative
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
                && relative.components().next().is_some(),
            "recorded source path `{}` does not rebase to a path inside the \
             materialization root",
            path.display()
        );
        anyhow::ensure!(
            seen.insert(relative.clone()),
            "two recorded modules rebase onto `{}`",
            relative.display()
        );
        out.push(relative);
    }
    Ok(out)
}

/// The longest shared directory prefix of `paths`, `None` when empty.
fn common_ancestor<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Option<PathBuf> {
    let mut paths = paths.into_iter();
    let mut shared: Vec<std::path::Component<'a>> = paths.next()?.components().collect();
    for path in paths {
        let other: Vec<std::path::Component<'a>> = path.components().collect();
        let keep = shared
            .iter()
            .zip(&other)
            .take_while(|(a, b)| a == b)
            .count();
        shared.truncate(keep);
    }
    Some(shared.iter().collect())
}

/// Write one materialized module, skipping the write when the file already
/// holds exactly this text (the re-wake path).
fn materialize_file(destination: &Path, text: &str) -> Result<()> {
    if std::fs::read_to_string(destination).is_ok_and(|existing| existing == text) {
        return Ok(());
    }
    write_file_creating_dirs(destination, text.as_bytes())
        .with_context(|| format!("materializing {}", destination.display()))
}

/// Per-file change classification between a commit and its parent, for
/// `chidori history` listings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeChange {
    Added,
    Modified,
    Removed,
}

/// Diff two trees by path: `(path, change)` for every difference, sorted.
pub fn tree_changes(
    parent: Option<&SourceCommit>,
    commit: &SourceCommit,
) -> Vec<(PathBuf, TreeChange)> {
    let parent_tree: BTreeMap<&Path, &str> = parent
        .map(|parent| {
            parent
                .tree
                .iter()
                .map(|entry| (entry.path.as_path(), entry.object.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let mut changes = Vec::new();
    for entry in &commit.tree {
        match parent_tree.get(entry.path.as_path()) {
            None => changes.push((entry.path.clone(), TreeChange::Added)),
            Some(object) if *object != entry.object => {
                changes.push((entry.path.clone(), TreeChange::Modified));
            }
            Some(_) => {}
        }
    }
    for (path, _) in parent_tree {
        if commit.tree_object(path).is_none() {
            changes.push((path.to_path_buf(), TreeChange::Removed));
        }
    }
    changes.sort_by(|a, b| a.0.cmp(&b.0));
    changes
}

/// Largest diffable file, in lines. The LCS table is O(n·m); agent modules
/// are small, but a pathological input must degrade to a notice, not an
/// O(n²) memory spike.
const MAX_DIFF_LINES: usize = 20_000;

/// Render a unified diff (3 lines of context) between two source texts —
/// dependency-free, line-based LCS. Returns an empty string when the texts
/// are identical.
pub fn unified_diff(old: &str, new: &str, old_label: &str, new_label: &str) -> String {
    if old == new {
        return String::new();
    }
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    if old_lines.len() > MAX_DIFF_LINES || new_lines.len() > MAX_DIFF_LINES {
        return format!(
            "--- {old_label}\n+++ {new_label}\n(files differ; too large to diff: {} vs {} lines)\n",
            old_lines.len(),
            new_lines.len()
        );
    }

    // LCS lengths table (one extra row/col of zeros), then a backward walk
    // yields the aligned edit script.
    let n = old_lines.len();
    let m = new_lines.len();
    let mut lcs = vec![0u32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[at(i, j)] = if old_lines[i] == new_lines[j] {
                lcs[at(i + 1, j + 1)] + 1
            } else {
                lcs[at(i + 1, j)].max(lcs[at(i, j + 1)])
            };
        }
    }

    #[derive(PartialEq)]
    enum Op {
        Keep,
        Del,
        Add,
    }
    let mut script: Vec<(Op, usize)> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if old_lines[i] == new_lines[j] {
            script.push((Op::Keep, i));
            i += 1;
            j += 1;
        } else if lcs[at(i + 1, j)] >= lcs[at(i, j + 1)] {
            script.push((Op::Del, i));
            i += 1;
        } else {
            script.push((Op::Add, j));
            j += 1;
        }
    }
    while i < n {
        script.push((Op::Del, i));
        i += 1;
    }
    while j < m {
        script.push((Op::Add, j));
        j += 1;
    }

    // Group the script into hunks with up to 3 context lines on each side.
    const CONTEXT: usize = 3;
    let change_positions: Vec<usize> = script
        .iter()
        .enumerate()
        .filter(|(_, (op, _))| *op != Op::Keep)
        .map(|(pos, _)| pos)
        .collect();
    let mut out = format!("--- {old_label}\n+++ {new_label}\n");
    let mut hunk_start = 0usize;
    while hunk_start < change_positions.len() {
        // Extend the hunk while the next change is within 2*CONTEXT lines.
        let mut hunk_end = hunk_start;
        while hunk_end + 1 < change_positions.len()
            && change_positions[hunk_end + 1] - change_positions[hunk_end] <= CONTEXT * 2
        {
            hunk_end += 1;
        }
        let script_start = change_positions[hunk_start].saturating_sub(CONTEXT);
        let script_end = (change_positions[hunk_end] + CONTEXT + 1).min(script.len());

        // Hunk header coordinates: 1-based line numbers of the first old/new
        // line in the hunk (or the insertion point when a side is empty).
        let mut old_start = None;
        let mut new_start = None;
        let mut old_count = 0usize;
        let mut new_count = 0usize;
        let mut old_cursor = script[..script_start]
            .iter()
            .filter(|(op, _)| *op != Op::Add)
            .count();
        let mut new_cursor = script[..script_start]
            .iter()
            .filter(|(op, _)| *op != Op::Del)
            .count();
        let mut body = String::new();
        for (op, index) in &script[script_start..script_end] {
            match op {
                Op::Keep => {
                    old_start.get_or_insert(old_cursor + 1);
                    new_start.get_or_insert(new_cursor + 1);
                    body.push(' ');
                    body.push_str(old_lines[*index]);
                    old_cursor += 1;
                    new_cursor += 1;
                    old_count += 1;
                    new_count += 1;
                }
                Op::Del => {
                    old_start.get_or_insert(old_cursor + 1);
                    new_start.get_or_insert(new_cursor + 1);
                    body.push('-');
                    body.push_str(old_lines[*index]);
                    old_cursor += 1;
                    old_count += 1;
                }
                Op::Add => {
                    old_start.get_or_insert(old_cursor + 1);
                    new_start.get_or_insert(new_cursor + 1);
                    body.push('+');
                    body.push_str(new_lines[*index]);
                    new_cursor += 1;
                    new_count += 1;
                }
            }
            body.push('\n');
        }
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            if old_count == 0 {
                old_start.unwrap_or(1).saturating_sub(1)
            } else {
                old_start.unwrap_or(1)
            },
            old_count,
            if new_count == 0 {
                new_start.unwrap_or(1).saturating_sub(1)
            } else {
                new_start.unwrap_or(1)
            },
            new_count,
        ));
        out.push_str(&body);
        hunk_start = hunk_end + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::store::FsRunStore;

    fn temp_store() -> (std::path::PathBuf, FsRunStore) {
        let dir =
            std::env::temp_dir().join(format!("chidori-source-history-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        (dir.clone(), FsRunStore::new(dir))
    }

    fn commit_files(
        store: &FsRunStore,
        event: SourceCommitEvent,
        files: &[(PathBuf, String)],
        frontier: u64,
        extra_parent: Option<String>,
    ) -> Option<SourceCommit> {
        record_commit(
            store,
            CommitInput {
                event,
                run_id: "run-1",
                branch_id: None,
                entry_path: &files[0].0,
                files,
                journal_frontier: frontier,
                extra_parent,
                share_from: &[],
                backfill_cache: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn records_chain_with_dedup_and_content_addressing() {
        let (dir, store) = temp_store();
        let v1 = vec![
            (
                PathBuf::from("agent.ts"),
                "export const v = 1;\n".to_string(),
            ),
            (
                PathBuf::from("lib.ts"),
                "export const lib = true;\n".to_string(),
            ),
        ];
        let first = commit_files(&store, SourceCommitEvent::RunStart, &v1, 0, None).unwrap();
        assert_eq!(first.parents, Vec::<String>::new());
        assert_eq!(first.tree.len(), 2);

        // Identical tree → no new commit, no matter the event.
        assert!(
            commit_files(&store, SourceCommitEvent::ResumeSourceChange, &v1, 7, None).is_none()
        );
        assert_eq!(load_commits(&store).unwrap().len(), 1);

        // An edit chains onto the head; the unchanged module's object is shared.
        let v2 = vec![
            (
                PathBuf::from("agent.ts"),
                "export const v = 2;\n".to_string(),
            ),
            (
                PathBuf::from("lib.ts"),
                "export const lib = true;\n".to_string(),
            ),
        ];
        let second =
            commit_files(&store, SourceCommitEvent::ResumeSourceChange, &v2, 42, None).unwrap();
        assert_eq!(second.parents, vec![first.id.clone()]);
        assert_eq!(second.journal_frontier, 42);
        assert_eq!(
            second.tree_object(Path::new("lib.ts")),
            first.tree_object(Path::new("lib.ts"))
        );
        assert_ne!(
            second.tree_object(Path::new("agent.ts")),
            first.tree_object(Path::new("agent.ts"))
        );

        // Both versions of the entry are recoverable, verbatim.
        let old = load_object(&store, first.tree_object(Path::new("agent.ts")).unwrap())
            .unwrap()
            .unwrap();
        let new = load_object(&store, second.tree_object(Path::new("agent.ts")).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(old, "export const v = 1;\n");
        assert_eq!(new, "export const v = 2;\n");

        // Three unique objects on disk: two agent versions + one shared lib.
        let objects = store
            .list_blobs()
            .unwrap()
            .into_iter()
            .filter(|key| key.starts_with(SOURCE_OBJECTS_PREFIX))
            .count();
        assert_eq!(objects, 3);

        let changes = tree_changes(Some(&first), &second);
        assert_eq!(
            changes,
            vec![(PathBuf::from("agent.ts"), TreeChange::Modified)]
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fork_commit_carries_extra_parent() {
        let (dir, store) = temp_store();
        let files = vec![(PathBuf::from("double.ts"), "// strategy\n".to_string())];
        let fork = commit_files(
            &store,
            SourceCommitEvent::BranchFork,
            &files,
            2,
            Some("sha256:feedface".to_string()),
        )
        .unwrap();
        // Fresh branch store: the only parent is the parent run's head.
        assert_eq!(fork.parents, vec!["sha256:feedface".to_string()]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unified_diff_renders_hunks() {
        let old = "a\nb\nc\nd\ne\nf\ng\n";
        let new = "a\nb\nC!\nd\ne\nf\ng\nh\n";
        let diff = unified_diff(old, new, "a/x.ts@1111", "b/x.ts@2222");
        assert!(diff.contains("--- a/x.ts@1111"));
        assert!(diff.contains("+++ b/x.ts@2222"));
        assert!(diff.contains("-c\n"));
        assert!(diff.contains("+C!\n"));
        assert!(diff.contains("+h\n"));
        assert_eq!(unified_diff("same\n", "same\n", "a", "b"), "");
    }

    /// The recording hot path reads `HEAD.json`, not the whole log — and the
    /// log stays authoritative: a deleted/stale HEAD falls back to the log
    /// and is rebuilt by the next recording.
    #[test]
    fn head_cache_serves_recording_and_heals() {
        let (dir, store) = temp_store();
        let v1 = vec![(PathBuf::from("agent.ts"), "v1\n".to_string())];
        let v2 = vec![(PathBuf::from("agent.ts"), "v2\n".to_string())];
        let first = commit_files(&store, SourceCommitEvent::RunStart, &v1, 0, None).unwrap();
        let second =
            commit_files(&store, SourceCommitEvent::ResumeSourceChange, &v2, 3, None).unwrap();

        // HEAD is the newest commit, byte-parseable on its own.
        let head: SourceCommit =
            serde_json::from_slice(&store.get_blob(SOURCE_HEAD_FILE).unwrap().unwrap()).unwrap();
        assert_eq!(head.id, second.id);

        // Losing HEAD loses nothing: head_commit rebuilds from the log, and
        // the next recording chains onto the true head and rewrites HEAD.
        store.delete_blob(SOURCE_HEAD_FILE).unwrap();
        assert_eq!(head_commit(&store).unwrap().unwrap().id, second.id);
        let v3 = vec![(PathBuf::from("agent.ts"), "v3\n".to_string())];
        let third =
            commit_files(&store, SourceCommitEvent::ResumeSourceChange, &v3, 9, None).unwrap();
        assert_eq!(third.parents, vec![second.id.clone()]);
        let head: SourceCommit =
            serde_json::from_slice(&store.get_blob(SOURCE_HEAD_FILE).unwrap().unwrap()).unwrap();
        assert_eq!(head.id, third.id);
        assert_eq!(load_commits(&store).unwrap().len(), 3);
        let _ = (first, std::fs::remove_dir_all(dir));
    }

    /// Identical content shared between stores is hardlinked, not rewritten:
    /// one stored copy per machine (inode-verified), whether it comes from a
    /// sibling store (`share_from`) or the cross-run cache (`backfill_cache`).
    #[cfg(unix)]
    #[test]
    fn objects_dedupe_by_hardlink_across_stores_and_cache() {
        use std::os::unix::fs::MetadataExt;

        let (dir_a, store_a) = temp_store();
        let (dir_b, store_b) = temp_store();
        let cache =
            std::env::temp_dir().join(format!("chidori-obj-cache-{}", uuid::Uuid::new_v4()));
        let files = vec![(PathBuf::from("agent.ts"), "shared content\n".to_string())];
        let hex = object_hex("shared content\n");

        // A records with back-fill: object lands in A and in the cache,
        // hardlinked (same inode).
        record_commit(
            &store_a,
            CommitInput {
                event: SourceCommitEvent::RunStart,
                run_id: "run-a",
                branch_id: None,
                entry_path: &files[0].0,
                files: &files,
                journal_frontier: 0,
                extra_parent: None,
                share_from: &[],
                backfill_cache: Some(&cache),
            },
        )
        .unwrap()
        .unwrap();
        let a_object = dir_a.join(SOURCE_OBJECTS_PREFIX).join(&hex);
        let cache_object = cache.join(&hex);
        assert!(a_object.is_file() && cache_object.is_file());
        let a_ino = a_object.metadata().unwrap().ino();
        assert_eq!(cache_object.metadata().unwrap().ino(), a_ino);

        // B records the same content sharing from A: linked, not rewritten.
        record_commit(
            &store_b,
            CommitInput {
                event: SourceCommitEvent::RunStart,
                run_id: "run-b",
                branch_id: None,
                entry_path: &files[0].0,
                files: &files,
                journal_frontier: 0,
                extra_parent: None,
                share_from: &[dir_a.join(SOURCE_OBJECTS_PREFIX)],
                backfill_cache: None,
            },
        )
        .unwrap()
        .unwrap();
        let b_object = dir_b.join(SOURCE_OBJECTS_PREFIX).join(&hex);
        assert_eq!(b_object.metadata().unwrap().ino(), a_ino);
        assert_eq!(
            load_object(&store_b, &format!("sha256:{hex}"))
                .unwrap()
                .unwrap(),
            "shared content\n"
        );

        let _ = std::fs::remove_dir_all(dir_a);
        let _ = std::fs::remove_dir_all(dir_b);
        let _ = std::fs::remove_dir_all(cache);
    }

    /// The cross-run cache lives beside the standard `runs/` base — and only
    /// there, so arbitrary embedder/test bases never get a cache planted in
    /// their parent directory.
    #[test]
    fn cross_run_cache_requires_standard_layout() {
        assert_eq!(
            cross_run_cache(Path::new("/proj/.chidori/runs")),
            Some(PathBuf::from("/proj/.chidori/history-objects"))
        );
        assert_eq!(cross_run_cache(Path::new("/tmp/some-arbitrary-base")), None);
    }

    /// Materialization is the read side of the durable source: a node with
    /// none of the tree on disk rebuilds it from the head commit, at the
    /// recorded relative layout (so relative imports still resolve) and
    /// nowhere near the recording machine's absolute directories.
    #[test]
    fn materializes_the_recorded_tree_at_its_relative_layout() {
        let (dir, store) = temp_store();
        let files = vec![
            (
                PathBuf::from("/elsewhere/project/services/worker.ts"),
                "import { helper } from \"../lib/util.ts\";\n".to_string(),
            ),
            (
                PathBuf::from("/elsewhere/project/lib/util.ts"),
                "export const helper = 1;\n".to_string(),
            ),
        ];
        commit_files(&store, SourceCommitEvent::RunStart, &files, 0, None).unwrap();

        let root = dir.join("materialized");
        let entry = materialize_source(&store, &root).unwrap().unwrap();
        assert_eq!(entry, root.join("services/worker.ts"));
        assert_eq!(std::fs::read_to_string(&entry).unwrap(), files[0].1);
        assert_eq!(
            std::fs::read_to_string(root.join("lib/util.ts")).unwrap(),
            files[1].1
        );

        // Idempotent: a second wake reuses what is already there.
        assert_eq!(materialize_source(&store, &root).unwrap().unwrap(), entry);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Runs recorded before source history existed still carry their entry
    /// text in the snapshot blob's `DurableBlob.bundle` — enough to wake a
    /// single-module agent on a node that has no copy of it.
    #[test]
    fn materializes_from_the_snapshot_bundle_without_history() {
        use crate::runtime::snapshot::{
            RuntimePolicy, SnapshotAbi, SnapshotManifest, SourceFingerprint, SNAPSHOT_MANIFEST_FILE,
        };

        let (dir, store) = temp_store();
        let source = "export async function agent() { return 1; }\n";
        let entry_path = PathBuf::from("/gone/project/services/legacy.ts");
        let manifest = SnapshotManifest::new(
            "run-legacy",
            SnapshotAbi::current("chidori-quickjs"),
            RuntimePolicy::durable_default("run-legacy"),
            SourceFingerprint::from_source(&entry_path, source),
            Vec::new(),
            None,
            0,
        );
        let blob = chidori_js::replay::DurableBlob {
            bundle: source.to_string(),
            effects: Vec::new(),
            journal: Vec::new(),
            image: None,
        };
        store
            .put_blob(&manifest.snapshot_file, &serde_json::to_vec(&blob).unwrap())
            .unwrap();
        store
            .put_blob(
                SNAPSHOT_MANIFEST_FILE,
                &serde_json::to_vec(&manifest).unwrap(),
            )
            .unwrap();

        let root = dir.join("materialized");
        assert!(head_commit(&store).unwrap().is_none());
        let entry = materialize_source(&store, &root).unwrap().unwrap();
        assert_eq!(entry, root.join("legacy.ts"));
        assert_eq!(std::fs::read_to_string(&entry).unwrap(), source);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The store's paths are untrusted input to a path join: nothing it names
    /// may land outside the materialization root, and an absolute path is
    /// rebased into the root rather than followed.
    #[test]
    fn materialization_refuses_paths_that_escape_the_root() {
        let (dir, store) = temp_store();
        let escape = std::env::temp_dir().join("chidori-materialize-escape.ts");
        let _ = std::fs::remove_file(&escape);
        let files = vec![
            (PathBuf::from("agent.ts"), "ok\n".to_string()),
            (
                PathBuf::from("../../../../../../../..")
                    .join(escape.strip_prefix("/").unwrap_or(&escape)),
                "pwned\n".to_string(),
            ),
        ];
        commit_files(&store, SourceCommitEvent::RunStart, &files, 0, None).unwrap();

        let root = dir.join("materialized");
        let err = materialize_source(&store, &root).unwrap_err();
        assert!(
            format!("{err:#}").contains("escapes its tree"),
            "unexpected error: {err:#}"
        );
        assert!(!escape.exists(), "materialization wrote outside its root");

        // Absolute paths that share no ancestor with the tree are rebased
        // under the root, never written where they claim to live.
        let (dir2, store2) = temp_store();
        let files = vec![
            (PathBuf::from("/srv/app/agent.ts"), "ok\n".to_string()),
            (
                PathBuf::from("/etc/hostname"),
                "not-your-host\n".to_string(),
            ),
        ];
        commit_files(&store2, SourceCommitEvent::RunStart, &files, 0, None).unwrap();
        let root2 = dir2.join("materialized");
        let entry = materialize_source(&store2, &root2).unwrap().unwrap();
        assert_eq!(entry, root2.join("srv/app/agent.ts"));
        assert_eq!(
            std::fs::read_to_string(root2.join("etc/hostname")).unwrap(),
            "not-your-host\n"
        );

        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dir2);
    }

    /// The materialization root is a sibling of `runs/` (run enumeration must
    /// never mistake it for a run), and an unsafe run id is refused outright.
    #[test]
    fn materialization_root_is_a_sibling_of_the_run_base() {
        assert_eq!(
            materialization_root(Path::new("/proj/.chidori/runs"), "run-1").unwrap(),
            PathBuf::from("/proj/.chidori/materialized/run-1")
        );
        assert_eq!(
            materialization_root(Path::new("/tmp/base"), "run-1").unwrap(),
            PathBuf::from("/tmp/base/materialized/run-1")
        );
        assert!(materialization_root(Path::new("/proj/.chidori/runs"), "../escape").is_err());
        assert!(materialization_root(Path::new("/proj/.chidori/runs"), "").is_err());
    }

    #[test]
    fn commit_line_tolerates_truncated_tail() {
        let (dir, store) = temp_store();
        let files = vec![(PathBuf::from("agent.ts"), "x\n".to_string())];
        commit_files(&store, SourceCommitEvent::RunStart, &files, 0, None).unwrap();
        // Simulate a crash-truncated append.
        let mut bytes = store.get_blob(SOURCE_COMMITS_FILE).unwrap().unwrap();
        bytes.extend_from_slice(b"{\"id\":\"sha256:trunc");
        store.put_blob(SOURCE_COMMITS_FILE, &bytes).unwrap();
        assert_eq!(load_commits(&store).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
