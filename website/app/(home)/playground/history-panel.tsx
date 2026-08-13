'use client';

/**
 * The implementation-history panel: renders the playground's git-like chain
 * of agent.ts versions (source-history.ts) the way `chidori history` renders
 * a run's — newest first, each commit anchored to the user turns that
 * executed under it, with inline diffs against the parent version, one-click
 * restore (which itself becomes a commit, like `git revert`), and chips for
 * the stashed timelines whose code sits at each commit.
 */

import { useMemo, useState } from 'react';
import {
  activeChain,
  commitById,
  shortId,
  unifiedDiff,
  type PlaygroundCommit,
  type SourceHistoryStore,
} from './source-history';

const EVENT_LABEL: Record<PlaygroundCommit['event'], string> = {
  run_start: 'run start',
  source_change: 'source change',
  restore: 'restore',
};

const EVENT_STYLE: Record<PlaygroundCommit['event'], string> = {
  run_start: 'border-fd-border text-fd-muted-foreground',
  source_change: 'border-amber-500/50 text-amber-600 dark:text-amber-400',
  restore: 'border-sky-500/50 text-sky-600 dark:text-sky-400',
};

interface TimelineHead {
  label: string;
  head: string | undefined;
}

export function HistoryPanel({
  history,
  timelineHeads,
  activeLabel,
  currentTurns,
  busy,
  onRestore,
}: {
  history: SourceHistoryStore;
  timelineHeads: TimelineHead[];
  activeLabel: string;
  currentTurns: number;
  busy: boolean;
  onRestore: (commit: PlaygroundCommit) => void;
}) {
  /** commit id -> 'view' | 'diff' currently expanded. */
  const [open, setOpen] = useState<Record<string, 'view' | 'diff' | undefined>>({});

  const chain = useMemo(() => activeChain(history), [history]);
  const chainIndex = useMemo(() => {
    const index = new Map<string, number>();
    chain.forEach((commit, i) => index.set(commit.id, i));
    return index;
  }, [chain]);

  if (history.commits.length === 0) {
    return (
      <p className="mt-3 text-xs text-fd-muted-foreground">
        No versions recorded yet — send a message to record the starting
        implementation, then ask the agent to rewrite its code (or edit it
        under the hood) to grow the chain.
      </p>
    );
  }

  const objectCount = Object.keys(history.objects).length;
  const newestFirst = [...history.commits].reverse();

  /** The user turns that executed under a commit, on the active chain. */
  const turnRange = (commit: PlaygroundCommit): string | null => {
    const index = chainIndex.get(commit.id);
    if (index === undefined) return null; // a stashed timeline's commit
    const start = commit.turnFrontier + 1;
    const end = index + 1 < chain.length ? chain[index + 1].turnFrontier : currentTurns;
    if (end < start) return commit.id === history.head ? 'no turns yet' : 'no turns';
    return `turn${end > start ? `s ${start}–${end}` : ` ${start}`}`;
  };

  const toggle = (id: string, mode: 'view' | 'diff') => {
    setOpen((prev) => ({ ...prev, [id]: prev[id] === mode ? undefined : mode }));
  };

  const chip =
    'rounded-md border border-fd-border px-2 py-0.5 font-mono text-[10px] transition-colors hover:bg-fd-accent disabled:pointer-events-none disabled:opacity-40';

  return (
    <div className="mt-3">
      <p className="font-mono text-[11px] text-fd-muted-foreground">
        {history.commits.length} version{history.commits.length === 1 ? '' : 's'} ·{' '}
        {objectCount} unique source{objectCount === 1 ? '' : 's'} — identical
        versions share one stored copy (content-addressed, like the runtime&apos;s
        hardlinked history objects)
      </p>
      <ol className="mt-2 flex flex-col gap-1.5">
        {newestFirst.map((commit) => {
          const parent = commitById(history, commit.parents[0]);
          const range = turnRange(commit);
          const heads = timelineHeads.filter((t) => t.head === commit.id);
          const isHead = commit.id === history.head;
          const mode = open[commit.id];
          const text = history.objects[commit.object] ?? '';
          return (
            <li
              key={commit.id}
              className={`rounded-lg border px-2.5 py-2 ${
                isHead ? 'border-fd-primary/50 bg-fd-primary/5' : 'border-fd-border'
              }`}
            >
              <div className="flex flex-wrap items-center gap-x-2 gap-y-1 font-mono text-[11px]">
                <code className="text-fd-foreground">{shortId(commit.id)}</code>
                <span className={`rounded-full border px-2 py-px text-[10px] ${EVENT_STYLE[commit.event]}`}>
                  {EVENT_LABEL[commit.event]}
                </span>
                {commit.restores && (
                  <span className="text-fd-muted-foreground">⟲ of {shortId(commit.restores)}</span>
                )}
                {isHead && (
                  <span className="rounded-full border border-fd-primary/60 bg-fd-primary/10 px-2 py-px text-[10px] font-medium">
                    HEAD · {activeLabel}
                  </span>
                )}
                {heads.map((t) => (
                  <span
                    key={t.label}
                    className="rounded-full border border-fd-border px-2 py-px text-[10px] text-fd-muted-foreground"
                  >
                    ⑂ {t.label}
                  </span>
                ))}
                <span className="ml-auto flex items-center gap-1.5">
                  {range && <span className="text-fd-muted-foreground">{range}</span>}
                  <button
                    className={chip}
                    onClick={() => toggle(commit.id, 'diff')}
                    disabled={!parent}
                    title={
                      parent
                        ? 'Diff this version against its parent'
                        : 'The first version has no parent to diff against'
                    }
                  >
                    {mode === 'diff' ? '× diff' : 'diff'}
                  </button>
                  <button
                    className={chip}
                    onClick={() => toggle(commit.id, 'view')}
                    title="Show this version's full source"
                  >
                    {mode === 'view' ? '× view' : 'view'}
                  </button>
                  <button
                    className={chip}
                    onClick={() => onRestore(commit)}
                    disabled={isHead || busy}
                    title="Hot-swap this version back in — the journal replays against it, and the restore is recorded as a new commit"
                  >
                    restore
                  </button>
                </span>
              </div>
              {mode === 'view' && (
                <pre className="mt-2 max-h-64 overflow-auto rounded-md border border-fd-border bg-fd-card p-2.5 text-[11px] leading-relaxed">
                  {text}
                </pre>
              )}
              {mode === 'diff' && parent && (
                <pre className="mt-2 max-h-64 overflow-auto rounded-md border border-fd-border bg-fd-card p-2.5 text-[11px] leading-relaxed">
                  {unifiedDiff(history.objects[parent.object] ?? '', text).map(
                    (line, i) => (
                      <div
                        key={i}
                        className={
                          line.kind === 'add'
                            ? 'bg-emerald-500/10 text-emerald-700 dark:text-emerald-400'
                            : line.kind === 'del'
                              ? 'bg-rose-500/10 text-rose-700 dark:text-rose-400'
                              : 'text-fd-muted-foreground'
                        }
                      >
                        {line.kind === 'add' ? '+' : line.kind === 'del' ? '-' : ' '}
                        {line.text}
                      </div>
                    ),
                  )}
                </pre>
              )}
            </li>
          );
        })}
      </ol>
    </div>
  );
}
