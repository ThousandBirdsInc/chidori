'use client';

/**
 * The docs VM terminal: a hand-rolled interactive shell UI over the fake
 * Linux VM (vfs + shell + commands + the in-browser chidori CLI). Opened by
 * the Run button on the docs' shell blocks, it auto-types the block's
 * commands, executes them for real, and then leaves the reader at a live
 * prompt — history, Ctrl-C, Ctrl-L, pipes, the lot.
 */

import { useCallback, useEffect, useRef, useState } from 'react';
import { loadDocsIndex, loadEngine } from '../runner/assets';
import { makeDocsTools } from '../runner/host';
import { makeChidoriCommand } from './chidori-cli';
import { makeCoreCommands, makeCurl } from './commands';
import { routeRequest } from './server';
import { needsContinuation, runLine, spliceContinuation, type CommandIo, type ShellContext } from './shell';
import { HOME, PROJECT, loadVfs } from './vfs';

interface Chunk {
  text: string;
  cls: 'out' | 'err' | 'prompt' | 'typed' | 'dim';
}

interface PendingRead {
  prompt: string;
  resolve: (line: string) => void;
  reject: (err: Error) => void;
}

/** Turn a docs shell block into the lines the terminal auto-plays. */
export function scriptFromBlock(code: string): string[] {
  const lines: string[] = [];
  let buffer: string | null = null;
  for (const raw of code.replace(/\r\n/g, '\n').split('\n')) {
    const line = raw.replace(/^\$\s/, '');
    if (buffer !== null) {
      buffer = spliceContinuation(buffer, line);
    } else {
      if (!line.trim() || line.trim().startsWith('#')) continue;
      buffer = line;
    }
    if (!needsContinuation(buffer)) {
      lines.push(buffer);
      buffer = null;
    }
  }
  if (buffer !== null) lines.push(buffer);
  return lines;
}

export function VmTerminal({ script, autoFocus = true }: { script: string[]; autoFocus?: boolean }) {
  const [chunks, setChunks] = useState<Chunk[]>([]);
  const [ready, setReady] = useState(false);
  const [input, setInput] = useState('');
  const [prompt, setPrompt] = useState('');
  const [running, setRunning] = useState(false);
  const ctxRef = useRef<ShellContext | null>(null);
  const pendingReadRef = useRef<PendingRead | null>(null);
  const historyRef = useRef<string[]>([]);
  const historyPosRef = useRef(-1);
  const continuationRef = useRef<string | null>(null);
  const boxRef = useRef<HTMLDivElement | null>(null);
  const inputRef = useRef<HTMLInputElement | null>(null);
  const scriptQueueRef = useRef<string[]>([]);
  const mountedRef = useRef(true);

  const append = useCallback((text: string, cls: Chunk['cls']) => {
    if (!mountedRef.current) return;
    setChunks((prev) => {
      if (text.includes('\x0c')) return []; // `clear`
      const next = [...prev, { text, cls }];
      // Bound the scrollback so a chatty run can't grow without limit.
      return next.length > 4000 ? next.slice(next.length - 3000) : next;
    });
  }, []);

  const shellPrompt = useCallback(() => {
    const cwd = ctxRef.current?.cwd ?? PROJECT;
    const shown = cwd === HOME ? '~' : cwd.startsWith(`${HOME}/`) ? `~/${cwd.slice(HOME.length + 1)}` : cwd;
    return `user@chidori:${shown}$ `;
  }, []);

  const makeIo = useCallback(
    (): CommandIo => ({
      stdin: '',
      out: (t) => append(t, 'out'),
      err: (t) => append(t, 'err'),
      read: (readPrompt) =>
        new Promise<string>((resolve, reject) => {
          setPrompt(readPrompt);
          setInput('');
          pendingReadRef.current = {
            prompt: readPrompt,
            resolve: (line) => {
              pendingReadRef.current = null;
              append(`${readPrompt}${line}\n`, 'typed');
              resolve(line);
            },
            reject: (err) => {
              pendingReadRef.current = null;
              reject(err);
            },
          };
        }),
    }),
    [append],
  );

  const execute = useCallback(
    async (line: string) => {
      const ctx = ctxRef.current;
      if (!ctx) return;
      setRunning(true);
      ctx.interrupted.current = false;
      try {
        await runLine(line, ctx, makeIo());
      } catch (err) {
        append(`${String(err instanceof Error ? err.message : err)}\n`, 'err');
      }
      setRunning(false);
      setPrompt(shellPrompt());
    },
    [append, makeIo, shellPrompt],
  );

  const playNext = useCallback(async () => {
    const queue = scriptQueueRef.current;
    while (queue.length > 0 && mountedRef.current) {
      const line = queue.shift()!;
      // Typewriter, quick and interruptible.
      setPrompt(shellPrompt());
      for (let i = 1; i <= line.length; i += Math.max(1, Math.floor(line.length / 40))) {
        if (!mountedRef.current) return;
        setInput(line.slice(0, i));
        await new Promise((r) => setTimeout(r, 8));
      }
      setInput(line);
      await new Promise((r) => setTimeout(r, 120));
      setInput('');
      append(`${shellPrompt()}${line}\n`, 'typed');
      historyRef.current.push(line);
      await execute(line);
    }
  }, [append, execute, shellPrompt]);

  // Boot the VM once per mount; each Run click remounts with a fresh script.
  useEffect(() => {
    mountedRef.current = true;
    let cancelled = false;
    (async () => {
      const vfs = await loadVfs();
      if (cancelled) return;
      const docsIndexReady = loadDocsIndex();
      const commands = {
        ...makeCoreCommands(),
        curl: makeCurl((method, url, body) => routeRequest({ vfs, engine: loadEngine }, method, url, body)),
        chidori: makeChidoriCommand({
          engine: loadEngine,
          getDocsTools: () => {
            let index: Awaited<ReturnType<typeof loadDocsIndex>> = null;
            void docsIndexReady.then((i) => (index = i));
            return makeDocsTools(() => index);
          },
        }),
      };
      const ctx: ShellContext = {
        vfs,
        cwd: PROJECT,
        env: new Map([
          ['HOME', HOME],
          ['USER', 'user'],
          ['SHELL', '/bin/bash'],
          ['EDITOR', 'vi'],
          ['PATH', '/usr/local/bin:/usr/bin:/bin'],
        ]),
        commands,
        interrupted: { current: false },
      };
      ctxRef.current = ctx;
      setReady(true);
      append('chidori docs VM — a simulated Linux shell in your browser. The chidori CLI is real:\n', 'dim');
      append('agents execute on the wasm engine and journal to .chidori/runs/. Try `help`.\n\n', 'dim');
      setPrompt(shellPrompt());
      scriptQueueRef.current = [...script];
      void playNext();
    })();
    return () => {
      cancelled = true;
      mountedRef.current = false;
      if (ctxRef.current) ctxRef.current.interrupted.current = true;
      pendingReadRef.current?.reject(new Error('terminal closed'));
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const box = boxRef.current;
    if (box) box.scrollTop = box.scrollHeight;
  }, [chunks, input, prompt]);

  const onKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLInputElement>) => {
      const ctx = ctxRef.current;
      if (!ctx) return;
      if (e.key === 'c' && e.ctrlKey) {
        e.preventDefault();
        append(`${prompt}${input}^C\n`, 'typed');
        setInput('');
        continuationRef.current = null;
        ctx.interrupted.current = true;
        pendingReadRef.current?.reject(new Error('interrupted'));
        if (!running) setPrompt(shellPrompt());
        return;
      }
      if (e.key === 'l' && e.ctrlKey) {
        e.preventDefault();
        setChunks([]);
        return;
      }
      if (e.key === 'ArrowUp' || e.key === 'ArrowDown') {
        e.preventDefault();
        const history = historyRef.current;
        if (!history.length) return;
        let pos = historyPosRef.current < 0 ? history.length : historyPosRef.current;
        pos += e.key === 'ArrowUp' ? -1 : 1;
        pos = Math.max(0, Math.min(history.length, pos));
        historyPosRef.current = pos;
        setInput(pos === history.length ? '' : history[pos]);
        return;
      }
      if (e.key !== 'Enter') return;
      e.preventDefault();
      historyPosRef.current = -1;
      const line = input;
      setInput('');
      // A command is waiting on io.read (input(), y/a/N, signal>, chat REPL).
      if (pendingReadRef.current) {
        pendingReadRef.current.resolve(line);
        setPrompt('');
        return;
      }
      if (running) return;
      const buffer = continuationRef.current !== null ? spliceContinuation(continuationRef.current, line) : line;
      if (needsContinuation(buffer)) {
        append(`${continuationRef.current === null ? shellPrompt() : '> '}${line}\n`, 'typed');
        continuationRef.current = buffer;
        setPrompt('> ');
        return;
      }
      continuationRef.current = null;
      append(`${prompt}${line}\n`, 'typed');
      if (buffer.trim()) historyRef.current.push(buffer);
      void execute(buffer);
    },
    [append, execute, input, prompt, running, shellPrompt],
  );

  const clsMap: Record<Chunk['cls'], string> = {
    out: 'text-fd-foreground',
    err: 'text-red-400',
    prompt: 'text-emerald-500',
    typed: 'text-emerald-500',
    dim: 'text-fd-muted-foreground',
  };

  return (
    <div
      className="flex h-full min-h-0 flex-col rounded-lg border border-fd-border bg-black/90 font-mono text-[11.5px] leading-relaxed text-neutral-100 dark:bg-black/60"
      onClick={() => inputRef.current?.focus()}
      data-vm-terminal
    >
      <div ref={boxRef} className="min-h-0 flex-1 overflow-y-auto overscroll-contain whitespace-pre-wrap break-words p-2.5 [overflow-wrap:anywhere]">
        {chunks.map((c, i) => (
          <span key={i} className={c.cls === 'out' ? 'text-neutral-100' : clsMap[c.cls]}>
            {c.text}
          </span>
        ))}
        <span className="text-emerald-400">{prompt}</span>
        <span>{input}</span>
        <span className="animate-pulse">▌</span>
        {!ready && <span className="text-fd-muted-foreground">booting the docs VM…</span>}
      </div>
      {/* The real input is invisible: the terminal body renders the line. */}
      <input
        ref={inputRef}
        id="vm-terminal-input"
        type="text"
        value={input}
        onChange={(e) => setInput(e.target.value)}
        onKeyDown={onKeyDown}
        autoFocus={autoFocus}
        autoComplete="off"
        autoCapitalize="off"
        spellCheck={false}
        aria-label="Terminal input"
        className="h-0 w-0 opacity-0"
      />
    </div>
  );
}
