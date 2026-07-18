//! The durable artifact: an ordered, deterministically-keyed journal of host
//! effects. This is the *only* thing persisted — no VM state is ever serialized
//! (that is what makes modify-and-resume possible; see the plan's decision
//! record). On restore the bundle is re-evaluated and these recorded results are
//! fed back at each host call in order.

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::host::HostKey;

/// The settled result of one host operation.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum EffectOutcome {
    Resolved(Json),
    Rejected(String),
}

/// One journal record: a deterministic address plus the settled outcome.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JournalEntry {
    /// Call-site identifier (operation name, or author-supplied key).
    pub site: String,
    /// Per-site invocation index.
    pub seq: u64,
    /// The call's JSON arguments as recorded, so replay can detect an edit
    /// that kept the effect order but changed what an already-executed call
    /// asked for. `Null` for entries that carry no comparable args
    /// (`durableStep` memoization, journals written before args were
    /// recorded) — those skip the comparison.
    #[serde(default)]
    pub args: Json,
    pub outcome: EffectOutcome,
}

/// The serializable journal. `bundle_hash` pins the code the journal was
/// recorded against; restore re-evaluates that bundle before replaying.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Journal {
    pub bundle_hash: String,
    pub entries: Vec<JournalEntry>,
}

impl Journal {
    pub fn new(bundle_hash: impl Into<String>) -> Journal {
        Journal {
            bundle_hash: bundle_hash.into(),
            entries: Vec::new(),
        }
    }

    /// Look up a recorded outcome by deterministic key.
    pub fn lookup(&self, key: &HostKey) -> Option<&EffectOutcome> {
        self.entries
            .iter()
            .find(|e| e.site == key.site && e.seq == key.seq)
            .map(|e| &e.outcome)
    }

    pub fn append(&mut self, key: &HostKey, args: Json, outcome: EffectOutcome) {
        self.entries.push(JournalEntry {
            site: key.site.clone(),
            seq: key.seq,
            args,
            outcome,
        });
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Journal, String> {
        serde_json::from_slice(bytes).map_err(|e| e.to_string())
    }

    /// A simple content hash of a code bundle (FNV-1a over bytes). Used to pin
    /// the bundle in the journal so restore can detect a changed bundle.
    pub fn hash_bundle(src: &str) -> String {
        let mut hash: u64 = 0xcbf29ce484222325;
        for b in src.as_bytes() {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        format!("{hash:016x}")
    }
}
