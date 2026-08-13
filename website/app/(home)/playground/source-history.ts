/**
 * The playground's implementation history — the browser-side mirror of the
 * runtime's source history (docs/source-history.md): a git-like,
 * content-addressed record of every version of agent.ts that ran in this
 * tab, kept alongside the journaled conversation.
 *
 * Same model as the runtime's per-run store, scaled to one file and
 * localStorage:
 *
 * - **objects** — full source text stored once per unique content
 *   (sha-256). Rewriting the agent five times and reverting stores the
 *   distinct versions only; a revert reuses the original object (dedupe).
 * - **commits** — an append-only DAG: parent ids, the event that produced
 *   the version (`run_start`, `source_change`, `restore`), and
 *   `turnFrontier` — how many journaled user turns existed when the version
 *   took over. That anchor ties the code history to the execution history:
 *   turns (frontier, nextFrontier] ran on that commit's code.
 * - **timelines are branch heads** — every stashed timeline records the
 *   commit it runs, so switching paths moves HEAD without rewriting
 *   history, exactly like git branches over one object store.
 *
 * The store is a plain value; every mutation returns a new store, and the
 * caller persists it. Hashing uses WebCrypto (async) with a small FNV-1a
 * fallback for non-secure contexts.
 */

export type SourceHistoryEvent = 'run_start' | 'source_change' | 'restore';

export interface PlaygroundCommit {
  /** Hex commit id — sha-256 over parents + event + object + frontier. */
  id: string;
  /** Previous commit id(s) — first is the chain parent. */
  parents: string[];
  event: SourceHistoryEvent;
  /** Content address (hex) of the full source text in `objects`. */
  object: string;
  /** Journaled user turns that existed when this version took over. */
  turnFrontier: number;
  /** For `restore` commits: the id of the commit whose code was restored. */
  restores?: string;
  /** ISO timestamp — display only, not part of the id. */
  at: string;
}

export interface SourceHistoryStore {
  /** Content-addressed source texts: hex → full text, one per unique content. */
  objects: Record<string, string>;
  /** Every recorded commit, oldest first (the DAG's append-only log). */
  commits: PlaygroundCommit[];
  /** The active timeline's head commit id (null before anything ran). */
  head: string | null;
}

export function freshHistory(): SourceHistoryStore {
  return { objects: {}, commits: [], head: null };
}

/** Parse a persisted store, rejecting shapes that would break rendering. */
export function parseHistory(raw: string): SourceHistoryStore | null {
  try {
    const parsed = JSON.parse(raw) as SourceHistoryStore;
    if (
      parsed &&
      typeof parsed.objects === 'object' &&
      Array.isArray(parsed.commits) &&
      (parsed.head === null || typeof parsed.head === 'string')
    ) {
      return parsed;
    }
  } catch {
    /* corrupted — start fresh */
  }
  return null;
}

/** sha-256 hex via WebCrypto; FNV-1a 64 fallback off secure contexts. */
export async function hashText(text: string): Promise<string> {
  const subtle = globalThis.crypto?.subtle;
  if (subtle) {
    const digest = await subtle.digest('SHA-256', new TextEncoder().encode(text));
    return Array.from(new Uint8Array(digest))
      .map((b) => b.toString(16).padStart(2, '0'))
      .join('');
  }
  let hash = 0xcbf29ce484222325n;
  for (const byte of new TextEncoder().encode(text)) {
    hash ^= BigInt(byte);
    hash = (hash * 0x100000001b3n) & 0xffffffffffffffffn;
  }
  return `fnv1a64${hash.toString(16).padStart(16, '0')}`;
}

export function shortId(id: string): string {
  return id.slice(0, 12);
}

export function commitById(
  store: SourceHistoryStore,
  id: string | null | undefined,
): PlaygroundCommit | undefined {
  return id ? store.commits.find((c) => c.id === id) : undefined;
}

/**
 * Record a source version: dedupe against the head (recording the code the
 * head already runs is a no-op, so callers record unconditionally), store
 * the object once per unique content, append one commit, and advance HEAD.
 * Returns null on the no-op; otherwise the new store + commit.
 */
export async function recordCommit(
  store: SourceHistoryStore,
  event: SourceHistoryEvent,
  text: string,
  turnFrontier: number,
  options?: { restores?: string },
): Promise<{ store: SourceHistoryStore; commit: PlaygroundCommit } | null> {
  const object = await hashText(text);
  const head = commitById(store, store.head);
  if (head && head.object === object) return null;

  const parents = head ? [head.id] : [];
  const id = await hashText(
    ['playground-source-commit-v1', ...parents, event, object, String(turnFrontier)].join('\n'),
  );
  const commit: PlaygroundCommit = {
    id,
    parents,
    event,
    object,
    turnFrontier,
    ...(options?.restores ? { restores: options.restores } : {}),
    at: new Date().toISOString(),
  };
  return {
    store: {
      objects: store.objects[object] === undefined
        ? { ...store.objects, [object]: text }
        : store.objects,
      commits: [...store.commits, commit],
      head: id,
    },
    commit,
  };
}

/** Move HEAD to an existing commit (timeline switch) — history untouched. */
export function moveHead(
  store: SourceHistoryStore,
  head: string | null,
): SourceHistoryStore {
  return { ...store, head };
}

/**
 * The active timeline's chain, oldest first: HEAD then first parents. This
 * is the lineage whose commits carry the executed turn ranges; commits off
 * it belong to stashed timelines.
 */
export function activeChain(store: SourceHistoryStore): PlaygroundCommit[] {
  const chain: PlaygroundCommit[] = [];
  const seen = new Set<string>();
  let cursor = commitById(store, store.head);
  while (cursor && !seen.has(cursor.id)) {
    seen.add(cursor.id);
    chain.push(cursor);
    cursor = commitById(store, cursor.parents[0]);
  }
  chain.reverse();
  return chain;
}

// --- unified diff (line-based LCS, 3 lines of context) ----------------------

export interface DiffLine {
  kind: 'hunk' | 'ctx' | 'add' | 'del';
  text: string;
}

/** Structured unified diff for rendering; empty when the texts match. */
export function unifiedDiff(oldText: string, newText: string): DiffLine[] {
  if (oldText === newText) return [];
  const a = oldText.split('\n');
  const b = newText.split('\n');

  // LCS lengths, then a forward walk into an aligned edit script.
  const n = a.length;
  const m = b.length;
  const lcs = new Uint32Array((n + 1) * (m + 1));
  const at = (i: number, j: number) => i * (m + 1) + j;
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      lcs[at(i, j)] =
        a[i] === b[j]
          ? lcs[at(i + 1, j + 1)] + 1
          : Math.max(lcs[at(i + 1, j)], lcs[at(i, j + 1)]);
    }
  }
  type Op = { kind: 'ctx' | 'add' | 'del'; text: string };
  const script: Op[] = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) {
      script.push({ kind: 'ctx', text: a[i] });
      i++;
      j++;
    } else if (lcs[at(i + 1, j)] >= lcs[at(i, j + 1)]) {
      script.push({ kind: 'del', text: a[i++] });
    } else {
      script.push({ kind: 'add', text: b[j++] });
    }
  }
  while (i < n) script.push({ kind: 'del', text: a[i++] });
  while (j < m) script.push({ kind: 'add', text: b[j++] });

  // Group into hunks with up to 3 context lines around each change.
  const CONTEXT = 3;
  const changes = script
    .map((op, index) => (op.kind === 'ctx' ? -1 : index))
    .filter((index) => index >= 0);
  const out: DiffLine[] = [];
  let hunkStart = 0;
  while (hunkStart < changes.length) {
    let hunkEnd = hunkStart;
    while (
      hunkEnd + 1 < changes.length &&
      changes[hunkEnd + 1] - changes[hunkEnd] <= CONTEXT * 2
    ) {
      hunkEnd += 1;
    }
    const from = Math.max(0, changes[hunkStart] - CONTEXT);
    const to = Math.min(script.length, changes[hunkEnd] + CONTEXT + 1);
    let oldLine = 1;
    let newLine = 1;
    for (let k = 0; k < from; k++) {
      if (script[k].kind !== 'add') oldLine += 1;
      if (script[k].kind !== 'del') newLine += 1;
    }
    out.push({ kind: 'hunk', text: `@@ -${oldLine} +${newLine} @@` });
    for (let k = from; k < to; k++) out.push(script[k] as DiffLine);
    hunkStart = hunkEnd + 1;
  }
  return out;
}
