//! Native npm package management: `chidori add` / `chidori install` /
//! `chidori remove`.
//!
//! Agents import npm packages through the Node-style resolver in
//! `runtime::typescript::resolver`, which reads from `node_modules`. This
//! module populates that directory without requiring Node, npm, or bun on the
//! machine — the same "one Rust binary" stance as the rest of chidori.
//!
//! Design (see docs/package-management.md):
//! - **Content-addressed global store** at `~/.chidori/cache/packages`:
//!   each package version is downloaded and extracted exactly once, keyed by
//!   its registry integrity hash, then materialized into a project's
//!   `node_modules` by hardlinking (copy fallback). Warm installs touch no
//!   network and duplicate no file contents.
//! - **Integrity verification**: every tarball is verified against the
//!   registry's `sha512` integrity (or legacy `sha1` shasum) before it enters
//!   the store. Hashing runs on blocking worker threads, off the async
//!   download path.
//! - **Sorted JSONL lockfile** (`chidori.lock.jsonl`): one JSON object per
//!   line, strictly sorted, so concurrent dependency changes merge in git
//!   without conflict churn.
//! - **No lifecycle scripts**: `preinstall`/`postinstall` never run. Installs
//!   are pure data movement, which removes the single largest npm supply-chain
//!   attack vector. Packages needing native builds don't apply here — agent
//!   code runs on chidori's embedded engine.

pub mod compat;
pub mod install;
pub mod layout;
pub mod lockfile;
pub mod manifest;
pub mod registry;
pub mod resolve;
pub mod store;

pub use install::{cmd_add, cmd_install, cmd_remove};
