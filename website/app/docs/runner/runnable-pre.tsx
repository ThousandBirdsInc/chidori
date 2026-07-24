'use client';

/**
 * The docs site's code-block renderer: fumadocs' default CodeBlock, plus a
 * "Run" button on every block that the build-time analysis
 * (scripts/build-runnable-examples.mjs) proved can execute in the browser
 * sandbox. Clicking it opens the example-runner side panel with this
 * block's code.
 *
 * The block's text is read from the rendered DOM (shiki preserves the code
 * verbatim) and matched against the runnable index by content hash — no
 * per-block annotations in the markdown, which must stay readable on GitHub.
 */

import { CodeBlock, Pre } from 'fumadocs-ui/components/codeblock';
import { type ComponentProps, useEffect, useRef, useState } from 'react';
import { lookupRunnable, openRunner, type RunnableInfo } from './runner-store';

export function RunnablePre(props: ComponentProps<'pre'>) {
  const boxRef = useRef<HTMLDivElement | null>(null);
  const codeRef = useRef('');
  const [runnable, setRunnable] = useState<(RunnableInfo & { id: string }) | null>(null);

  useEffect(() => {
    const pre = boxRef.current?.querySelector('pre');
    if (!pre) return;
    const code = pre.textContent ?? '';
    codeRef.current = code;
    let cancelled = false;
    lookupRunnable(code).then((hit) => {
      if (hit && !cancelled) setRunnable(hit);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <div ref={boxRef} className="relative">
      <CodeBlock {...props}>
        <Pre>{props.children}</Pre>
      </CodeBlock>
      {runnable && (
        <button
          type="button"
          data-runnable-example={runnable.id}
          className="absolute bottom-2 right-2 inline-flex h-7 items-center gap-1 rounded-full border border-fd-primary/50 bg-fd-background/90 px-2.5 text-xs font-medium shadow-sm backdrop-blur transition-colors hover:bg-fd-accent"
          title="Run this example on the wasm chidori engine, right here in your browser"
          onClick={() =>
            openRunner({
              ...runnable,
              code: codeRef.current,
              title: document.title.split('|')[0].trim() || 'Docs example',
            })
          }
        >
          ▶ Run
        </button>
      )}
    </div>
  );
}
