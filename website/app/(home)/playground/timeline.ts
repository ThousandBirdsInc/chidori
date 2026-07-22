/**
 * Journal surgery for the playground's rewind & branch controls.
 *
 * A durable blob is self-describing JSON — {bundle, effects, journal} — and
 * the journal is itself JSON: {bundle_hash, entries: [{site, seq, args,
 * outcome}]}. Every chat turn begins with exactly one `input` effect (the
 * agent's `chidori.input()` call), so "rewind to before turn N" is pure data
 * work: cut the entry list just before the Nth `input` entry. Restoring the
 * shorter blob replays the surviving prefix offline and blocks at that
 * `chidori.input()` — the exact suspended state the run was in before the
 * user sent message N. Branching is the same cut after stashing the
 * full-length blob, so the discarded future remains a switchable timeline.
 */

interface JournalEntry {
  site: string;
  seq: number;
  args?: unknown;
  outcome?: unknown;
}

interface JournalDoc {
  bundle_hash: string;
  entries: JournalEntry[];
}

interface BlobDoc {
  bundle: string;
  effects: string[];
  /** Journal bytes; serde's Vec<u8> serializes as a JSON number array. */
  journal: number[];
}

function decodeJournal(blob: BlobDoc): JournalDoc {
  return JSON.parse(new TextDecoder().decode(new Uint8Array(blob.journal)));
}

/** How many user turns (journaled `chidori.input()` results) the blob holds. */
export function countTurns(blobText: string): number {
  try {
    const journal = decodeJournal(JSON.parse(blobText));
    return journal.entries.filter((e) => e.site === 'input').length;
  } catch {
    return 0;
  }
}

/**
 * A copy of the blob rewound to just before user turn `turn` (0-based):
 * every journal entry from that turn's `input` onward is dropped, everything
 * else — bundle, effects, bundle_hash, earlier entries — is preserved
 * verbatim. Returns null when the blob does not parse or has no such turn.
 */
export function truncateAtTurn(blobText: string, turn: number): string | null {
  try {
    const blob: BlobDoc = JSON.parse(blobText);
    const journal = decodeJournal(blob);
    let inputs = 0;
    let cut = -1;
    for (let i = 0; i < journal.entries.length; i++) {
      if (journal.entries[i].site === 'input') {
        if (inputs === turn) {
          cut = i;
          break;
        }
        inputs += 1;
      }
    }
    if (cut < 0) return null;
    journal.entries = journal.entries.slice(0, cut);
    blob.journal = Array.from(new TextEncoder().encode(JSON.stringify(journal)));
    return JSON.stringify(blob);
  } catch {
    return null;
  }
}

/** One stashed (inactive) timeline. */
export interface Branch {
  label: string;
  blob: string;
  turns: number;
  /**
   * The agent source this timeline was running when stashed (its blob's
   * bundle is the *transpiled* form, so the readable source rides along for
   * display; absent on branches stashed before self-modification existed).
   */
  source?: string;
}

/** The persisted branch set: the active path's label plus stashed timelines. */
export interface BranchStore {
  activeLabel: string;
  /** Monotonic counter for fresh "path N" labels (survives deletes). */
  nextId: number;
  stashed: Branch[];
}

export function freshBranches(): BranchStore {
  return { activeLabel: 'main', nextId: 2, stashed: [] };
}
