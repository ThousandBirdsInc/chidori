'use client';

/**
 * The editable "under the hood" source view: a CodeMirror editor over the
 * agent's live source, with an Apply button that funnels manual edits
 * through the exact same validate → hot-swap path the chat's update_source
 * tool uses. The heavy editor chunk loads only once the panel is opened
 * (this component is mounted lazily by the client).
 */

import { useCallback, useEffect, useState } from 'react';
import CodeMirror from '@uiw/react-codemirror';
import { javascript } from '@codemirror/lang-javascript';

/** Tracks fumadocs' theme, which toggles the `dark` class on <html>. */
function useIsDark(): boolean {
  const [dark, setDark] = useState(false);
  useEffect(() => {
    const el = document.documentElement;
    const update = () => setDark(el.classList.contains('dark'));
    update();
    const observer = new MutationObserver(update);
    observer.observe(el, { attributes: true, attributeFilter: ['class'] });
    return () => observer.disconnect();
  }, []);
  return dark;
}

const EXTENSIONS = [javascript({ typescript: true })];

export function SourceEditor({
  source,
  defaultSource,
  busy,
  onApply,
}: {
  /** The source the agent is currently running (chat edits move it). */
  source: string;
  defaultSource: string;
  /** True while a turn is in flight — applying mid-turn would race the agent. */
  busy: boolean;
  /** Validate + hot-swap; returns an error message to display, or null. */
  onApply: (next: string) => string | null;
}) {
  const [draft, setDraft] = useState(source);
  const [error, setError] = useState<string | null>(null);
  const isDark = useIsDark();

  // When the *agent's* source changes under us (a chat-driven swap, a branch
  // switch, a reset), follow it — unless the user has unsaved edits, which
  // must not be clobbered by the agent's own rewrite.
  const [lastSynced, setLastSynced] = useState(source);
  if (source !== lastSynced) {
    setLastSynced(source);
    if (draft === lastSynced) setDraft(source);
  }
  const dirty = draft !== source;

  const apply = useCallback(() => {
    setError(onApply(draft));
  }, [onApply, draft]);

  const button =
    'rounded-lg border border-fd-border px-3 py-1.5 text-xs font-medium transition-colors hover:bg-fd-accent disabled:pointer-events-none disabled:opacity-40';

  return (
    <div className="mt-1">
      <div className="overflow-hidden rounded-lg border border-fd-border text-xs [&_.cm-editor]:bg-transparent [&_.cm-focused]:outline-none">
        <CodeMirror
          value={draft}
          onChange={setDraft}
          extensions={EXTENSIONS}
          theme={isDark ? 'dark' : 'light'}
          maxHeight="24rem"
          basicSetup={{ foldGutter: false, searchKeymap: false }}
          aria-label="Agent source editor"
        />
      </div>
      <div className="mt-2 flex flex-wrap items-center gap-2">
        <button id="apply-source" className={button} disabled={!dirty || busy} onClick={apply}>
          🧬 Apply &amp; hot-swap
        </button>
        <button
          id="discard-source"
          className={button}
          disabled={!dirty}
          onClick={() => {
            setDraft(source);
            setError(null);
          }}
        >
          Discard edits
        </button>
        {draft !== defaultSource && (
          <button id="load-default-source" className={button} onClick={() => setDraft(defaultSource)}>
            Load original
          </button>
        )}
        {dirty && !error && (
          <span className="text-xs text-fd-muted-foreground">
            {busy ? 'unsaved edits — waiting for the current turn to finish' : 'unsaved edits'}
          </span>
        )}
      </div>
      {error && (
        <p id="apply-error" className="mt-2 text-xs text-red-500 dark:text-red-400">
          {error}
        </p>
      )}
    </div>
  );
}
