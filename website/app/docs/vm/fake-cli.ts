'use client';

/**
 * The faked half of the docs VM's `chidori` CLI: when the wasm engine can't
 * load (assets not built, wasm blocked, ancient browser), CLI actions still
 * play out — as a scripted walk of the agent's source instead of a real
 * execution. The fake reads the file's `chidori.*` calls in order, emits
 * the same journal-event shapes the real path produces (so `trace`,
 * `resume`, and `verify` keep working against the recorded journal), keeps
 * `input()` genuinely interactive, and even makes real model calls when
 * OpenRouter is connected — prompting needs no engine. Every faked run is
 * labelled as such; it never pretends to have executed anything.
 */

import type { Json } from '../../(home)/playground/brain';
import { decidePrompt } from '../runner/host';

/** One journal line, same shapes the harness feeds (host.ts RunEvent). */
export type FakeEvent = Record<string, Json>;

/** First string-literal argument of a call, if the source spells one. */
function literalArg(source: string, from: number): string | null {
  const m = source.slice(from).match(/^\s*\(\s*(["'`])((?:\\.|(?!\1)[^\\])*)\1/);
  return m ? m[2].replace(/\\n/g, '\n').replace(/\\(["'`])/g, '$1') : null;
}

/** The `choices: [...]` strings of an input() options literal, if present. */
function choicesNear(source: string, from: number): string[] {
  const window = source.slice(from, from + 400);
  const m = window.match(/choices\s*:\s*\[([^\]]*)\]/);
  if (!m) return [];
  return [...m[1].matchAll(/["'`]([^"'`]+)["'`]/g)].map((x) => x[1]);
}

/**
 * Plausible run output for the seeded flagship agents; anything else gets
 * an honest generic object. `answers` are the reader's input() replies, in
 * order; `replies` the prompt replies.
 */
export function fakeOutput(
  fileBase: string,
  input: Record<string, Json>,
  answers: string[],
  replies: string[],
): Json {
  const yes = (a: string | undefined) => (a ?? '').trim().toLowerCase() === 'yes';
  switch (fileBase) {
    case 'hello.ts':
      return { greeting: `Hello, ${String(input.name ?? 'world')}!` };
    case 'input_pause.ts':
      return { request: input.request ?? null, approved: yes(answers[0]) };
    case 'research.ts':
      return { answer: replies[replies.length - 1] ?? '(no prompt reply)', published: yes(answers[0]) };
    default:
      return { ok: true, faked: true, note: 'scripted output — the wasm engine was unavailable, so nothing actually executed' };
  }
}

export interface FakeRunResult {
  events: FakeEvent[];
  output: Json;
  failed: boolean;
}

/**
 * Walk the agent source's `chidori.*` calls in order and produce a
 * plausible journal. `readLine` keeps input() interactive; `note` narrates.
 */
export async function fakeRun(
  code: string,
  fileBase: string,
  input: Record<string, Json>,
  hooks: {
    readLine: (prompt: string) => Promise<string>;
    emit: (ev: FakeEvent) => void;
    writeFile?: (path: string, content: string) => void;
  },
): Promise<FakeRunResult> {
  const events: FakeEvent[] = [];
  const answers: string[] = [];
  const replies: string[] = [];
  const emit = (ev: FakeEvent) => {
    events.push(ev);
    hooks.emit(ev);
  };

  const calls = [...code.matchAll(/\bchidori\s*\.\s*(\w+)(?:\s*\.\s*(\w+))?/g)];
  for (const m of calls.slice(0, 24)) {
    const method = m[1];
    const sub = m[2] ?? null;
    const argAt = m.index + m[0].length;
    switch (method) {
      case 'log': {
        const msg = literalArg(code, argAt) ?? 'progress';
        emit({ k: 'log', message: msg, fields: null });
        break;
      }
      case 'prompt': {
        const text = literalArg(code, argAt) ?? `(prompt from ${fileBase})`;
        let reply: string;
        try {
          reply = await decidePrompt({ text, opts: {} });
        } catch {
          reply = '(faked prompt reply)';
        }
        replies.push(reply);
        emit({ k: 'prompt', text, reply });
        break;
      }
      case 'input': {
        const prompt = literalArg(code, argAt) ?? 'Continue?';
        const choices = choicesNear(code, argAt);
        const suffix = choices.length ? ` [${choices.join('/')}]` : '';
        const answer = await hooks.readLine(`${prompt}${suffix} `);
        answers.push(answer);
        emit({ k: 'input', prompt, answer });
        break;
      }
      case 'tool': {
        const name = literalArg(code, argAt) ?? 'tool';
        emit({ k: 'tool', name, args: {}, result: { faked: true } });
        break;
      }
      case 'template': {
        const label = literalArg(code, argAt) ?? '(inline template)';
        emit({ k: 'op', op: 'template', label, data: null });
        break;
      }
      case 'signal':
      case 'receive':
      case 'pollSignal': {
        const name = literalArg(code, argAt) ?? 'signal';
        emit({ k: 'signal', phase: 'received', names: [name], result: { name, payload: { faked: true }, from: { kind: 'human', id: 'faked' } } });
        break;
      }
      case 'alarm':
        emit({ k: 'op', op: 'alarm', label: 'fast-forwarded (faked)', data: null });
        break;
      case 'workspace': {
        if (sub === 'write') {
          const path = literalArg(code, argAt) ?? 'out.txt';
          hooks.writeFile?.(path, `(content faked — the wasm engine was unavailable when this run was recorded)\n`);
          emit({ k: 'op', op: 'workspace.write', label: path, data: { path, status: 'complete', sha256: 'faked', bytes: 0 } });
        }
        break;
      }
      case 'memory':
        if (sub) emit({ k: 'op', op: `memory.${sub}`, label: '(faked)', data: null });
        break;
      case 'actors':
      case 'agents':
        if (sub === 'spawn') {
          const source = literalArg(code, argAt) ?? 'worker.ts';
          emit({ k: 'op', op: `${method}.spawn`, label: `${source} (faked — not actually running)`, data: null });
        }
        break;
      case 'branch':
        emit({ k: 'op', op: 'branch', label: '(faked — variants not executed)', data: null });
        break;
      default:
        break;
    }
  }

  const output = fakeOutput(fileBase, input, answers, replies);
  emit({ k: 'result', value: output });
  emit({ k: 'done' });
  return { events, output, failed: false };
}

/** The pending input() prompt a faked server session would pause at. */
export function fakePendingPrompt(code: string): string | null {
  const m = code.match(/\bchidori\s*\.\s*input\s*/);
  return m ? (literalArg(code, m.index! + m[0].length - 0) ?? 'Approve this request?') : null;
}
