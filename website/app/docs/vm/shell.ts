'use client';

/**
 * The docs VM's shell: a small POSIX-flavoured interpreter over the virtual
 * filesystem — enough bash for every command the docs' code blocks run.
 * Quoting ('', "", \), $VAR / ${VAR} expansion, $(command substitution),
 * pipelines, && / || / ;, output redirection, globs, comments, and
 * line continuations. No job control — "background" servers are faked by
 * the chidori CLI itself.
 */

import { resolvePath, type Vfs } from './vfs';

/** Where a command's text goes: the terminal, a pipe, or a redirect. */
export interface CommandIo {
  out: (text: string) => void;
  err: (text: string) => void;
  /** Read one interactive line (chidori.input, y/a/N gates, chat REPL). */
  read: (prompt: string) => Promise<string>;
  stdin: string;
}

export interface ShellContext {
  vfs: Vfs;
  cwd: string;
  env: Map<string, string>;
  /** The command table (coreutils + chidori + friends). */
  commands: Record<string, ShellCommand>;
  /** Interrupt flag — set by Ctrl-C in the terminal. */
  interrupted: { current: boolean };
}

export type ShellCommand = (
  argv: string[],
  io: CommandIo,
  ctx: ShellContext,
) => Promise<number> | number;

interface Word {
  text: string;
  /** Single-quoted words never expand or glob. */
  literal: boolean;
}

interface ParsedCommand {
  assigns: [string, string][];
  words: Word[];
  redirectOut: { target: string; append: boolean } | null;
}

interface Pipeline {
  commands: ParsedCommand[];
  /** '&&' | '||' | ';' — how this pipeline chains to the NEXT one. */
  chain: '&&' | '||' | ';';
}

const isSpace = (c: string) => c === ' ' || c === '\t';

/**
 * Tokenize one logical line into pipelines of commands. Throws on unclosed
 * quotes (the terminal turns that into a continuation prompt).
 */
function parseLine(line: string): Pipeline[] {
  const pipelines: Pipeline[] = [];
  let commands: ParsedCommand[] = [];
  let current: ParsedCommand = { assigns: [], words: [], redirectOut: null };
  let word = '';
  let wordLiteral = false;
  let hasWord = false;
  let redirect: { append: boolean } | null = null;
  let i = 0;

  const pushWord = () => {
    if (!hasWord) return;
    if (redirect) {
      current.redirectOut = { target: word, append: redirect.append };
      redirect = null;
    } else if (current.words.length === 0 && !wordLiteral && /^[A-Za-z_][A-Za-z0-9_]*=/.test(word)) {
      const eq = word.indexOf('=');
      current.assigns.push([word.slice(0, eq), word.slice(eq + 1)]);
    } else {
      current.words.push({ text: word, literal: wordLiteral });
    }
    word = '';
    wordLiteral = false;
    hasWord = false;
  };
  const pushCommand = () => {
    pushWord();
    if (current.words.length > 0 || current.assigns.length > 0) commands.push(current);
    current = { assigns: [], words: [], redirectOut: null };
  };
  const pushPipeline = (chain: '&&' | '||' | ';') => {
    pushCommand();
    if (commands.length > 0) pipelines.push({ commands, chain });
    commands = [];
  };

  while (i < line.length) {
    const c = line[i];
    if (c === "'") {
      const end = line.indexOf("'", i + 1);
      if (end < 0) throw new Error('unclosed quote');
      word += line.slice(i + 1, end);
      wordLiteral = wordLiteral || word === line.slice(i + 1, end);
      hasWord = true;
      i = end + 1;
      continue;
    }
    if (c === '"') {
      let j = i + 1;
      let buf = '';
      for (; j < line.length && line[j] !== '"'; j++) {
        if (line[j] === '\\' && j + 1 < line.length && '"$\\`'.includes(line[j + 1])) {
          buf += line[j + 1];
          j++;
        } else buf += line[j];
      }
      if (j >= line.length) throw new Error('unclosed quote');
      // \x01…\x02 sentinels: expand $ inside, but never glob or drop-if-empty.
      word += `\x01${buf}\x02`;
      hasWord = true;
      i = j + 1;
      continue;
    }
    if (c === '\\' && i + 1 < line.length) {
      word += line[i + 1];
      hasWord = true;
      i += 2;
      continue;
    }
    if (c === '$' && line[i + 1] === '(') {
      // Command substitution is one unit: spaces and pipes inside $(…)
      // belong to the inner command, not this line's structure.
      let depth = 1;
      let j = i + 2;
      for (; j < line.length && depth > 0; j++) {
        if (line[j] === '(') depth++;
        else if (line[j] === ')') depth--;
      }
      word += line.slice(i, j);
      hasWord = true;
      i = j;
      continue;
    }
    if (c === '#' && !hasWord) break;
    if (isSpace(c)) {
      pushWord();
      i++;
      continue;
    }
    if (c === '|' && line[i + 1] === '|') {
      pushPipeline('||');
      i += 2;
      continue;
    }
    if (c === '|') {
      pushCommand();
      i++;
      continue;
    }
    if (c === '&' && line[i + 1] === '&') {
      pushPipeline('&&');
      i += 2;
      continue;
    }
    if (c === '&') {
      i++; // background & — the docs VM runs everything "fast enough"
      continue;
    }
    if (c === ';') {
      pushPipeline(';');
      i++;
      continue;
    }
    if (c === '>' ) {
      pushWord();
      redirect = { append: line[i + 1] === '>' };
      i += line[i + 1] === '>' ? 2 : 1;
      continue;
    }
    if (c === '<') {
      // Docs commands use `<run-id>`-style placeholders; treat them as
      // literal words, not input redirects (which the docs never use).
      const m = line.slice(i).match(/^<[^<>|&;]{1,60}>/);
      if (m) {
        word += m[0];
        hasWord = true;
        i += m[0].length;
        continue;
      }
      i++; // bare `< file`: the filename that follows becomes an argument
      continue;
    }
    if (c === '2' && line[i + 1] === '>' && !hasWord) {
      // 2> / 2>&1: the docs VM doesn't separate the streams — swallow it.
      i += line[i + 2] === '&' ? 4 : line[i + 1] === '>' ? 2 : 1;
      if (line[i] === '>') i++;
      continue;
    }
    word += c;
    hasWord = true;
    i++;
  }
  pushPipeline(';');
  return pipelines;
}

/** Expand $VAR, ${VAR}, and $(...) in a word (not single-quoted). */
async function expandWord(
  raw: string,
  literal: boolean,
  ctx: ShellContext,
  runCapture: (line: string) => Promise<string>,
): Promise<string> {
  if (literal) return raw;
  let out = '';
  let i = 0;
  while (i < raw.length) {
    const c = raw[i];
    if (c === '\x01' || c === '\x02') {
      i++; // quote sentinels — expansion applies inside, globbing never does
      continue;
    }
    if (c === '$') {
      if (raw[i + 1] === '(') {
        let depth = 1;
        let j = i + 2;
        for (; j < raw.length && depth > 0; j++) {
          if (raw[j] === '(') depth++;
          else if (raw[j] === ')') depth--;
        }
        const inner = raw.slice(i + 2, j - 1);
        out += (await runCapture(inner)).replace(/\n+$/, '');
        i = j;
        continue;
      }
      if (raw[i + 1] === '{') {
        const end = raw.indexOf('}', i + 2);
        if (end > 0) {
          out += ctx.env.get(raw.slice(i + 2, end)) ?? '';
          i = end + 1;
          continue;
        }
      }
      const m = raw.slice(i + 1).match(/^[A-Za-z_][A-Za-z0-9_]*|^\?/);
      if (m) {
        out += ctx.env.get(m[0]) ?? '';
        i += 1 + m[0].length;
        continue;
      }
    }
    out += c;
    i++;
  }
  return out;
}

/** Expand a `*` glob against the VFS; no match → the pattern itself. */
function expandGlob(word: string, ctx: ShellContext): string[] {
  if (!word.includes('*')) return [word];
  const abs = resolvePath(ctx.cwd, word);
  const dir = abs.slice(0, abs.lastIndexOf('/')) || '/';
  const pattern = abs.slice(abs.lastIndexOf('/') + 1);
  const re = new RegExp(`^${pattern.split('*').map((p) => p.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')).join('.*')}$`);
  const hits = ctx.vfs
    .list(dir)
    .filter((e) => re.test(e.name))
    .map((e) => (word.startsWith('/') ? `${dir}/${e.name}` : e.name));
  return hits.length ? hits : [word];
}

export interface RunLineResult {
  code: number;
}

/**
 * Execute one logical shell line. Output flows through `io`; interactive
 * reads (approval gates, chidori.input) come back through `io.read`.
 */
export async function runLine(line: string, ctx: ShellContext, io: CommandIo): Promise<RunLineResult> {
  const runCapture = async (sub: string): Promise<string> => {
    let captured = '';
    await runLine(sub, ctx, { ...io, out: (t) => (captured += t) });
    return captured;
  };

  let pipelines: Pipeline[];
  try {
    pipelines = parseLine(line);
  } catch (err) {
    io.err(`bash: ${String(err instanceof Error ? err.message : err)}\n`);
    return { code: 2 };
  }

  let lastCode = 0;
  let skipUntil: '&&' | '||' | null = null;
  for (const pipeline of pipelines) {
    if (skipUntil === '&&' && lastCode === 0) skipUntil = null;
    else if (skipUntil === '||' && lastCode !== 0) skipUntil = null;
    if (skipUntil) {
      skipUntil = pipeline.chain === ';' ? null : skipUntil;
      continue;
    }
    let pipedIn = '';
    for (let k = 0; k < pipeline.commands.length; k++) {
      if (ctx.interrupted.current) return { code: 130 };
      const cmd = pipeline.commands[k];
      const isLast = k === pipeline.commands.length - 1;
      const argv: string[] = [];
      for (const w of cmd.words) {
        const expanded = await expandWord(w.text, w.literal, ctx, runCapture);
        if (w.literal || w.text.includes('\x01')) argv.push(expanded);
        else if (expanded !== '') argv.push(...expandGlob(expanded, ctx));
      }
      // Assignment-only line: set variables and move on.
      if (argv.length === 0) {
        for (const [key, value] of cmd.assigns) {
          ctx.env.set(key, await expandWord(value, false, ctx, runCapture));
        }
        lastCode = 0;
        continue;
      }
      // Per-command env prefix (VAR=x cmd): apply for the call, then restore.
      const saved: [string, string | undefined][] = [];
      for (const [key, value] of cmd.assigns) {
        saved.push([key, ctx.env.get(key)]);
        ctx.env.set(key, await expandWord(value, false, ctx, runCapture));
      }
      let collected = '';
      const collectOut = (t: string) => (collected += t);
      const target = cmd.redirectOut;
      const cmdIo: CommandIo = {
        ...io,
        stdin: pipedIn,
        out: !isLast || target ? collectOut : io.out,
      };
      const impl = ctx.commands[argv[0]];
      try {
        if (!impl) {
          io.err(`bash: ${argv[0]}: command not found\n`);
          lastCode = 127;
        } else {
          lastCode = (await impl(argv, cmdIo, ctx)) ?? 0;
        }
      } catch (err) {
        io.err(`${argv[0]}: ${String(err instanceof Error ? err.message : err)}\n`);
        lastCode = 1;
      }
      for (const [key, value] of saved) {
        if (value === undefined) ctx.env.delete(key);
        else ctx.env.set(key, value);
      }
      if (target) {
        const abs = resolvePath(ctx.cwd, target.target);
        const prev = target.append ? (ctx.vfs.read(abs) ?? '') : '';
        ctx.vfs.write(abs, prev + collected);
        pipedIn = '';
      } else {
        pipedIn = collected;
      }
    }
    ctx.env.set('?', String(lastCode));
    if (pipeline.chain === '&&' && lastCode !== 0) skipUntil = '&&';
    if (pipeline.chain === '||' && lastCode === 0) skipUntil = '||';
  }
  return { code: lastCode };
}

/** A line ends with `\` → the terminal keeps reading before executing. */
export function needsContinuation(buffer: string): boolean {
  if (/\\\s*$/.test(buffer)) return true;
  // Unclosed quote → continuation too (the signals doc's multi-line curl -d).
  let inS = false;
  let inD = false;
  for (let i = 0; i < buffer.length; i++) {
    const c = buffer[i];
    if (c === '\\' && !inS) i++;
    else if (c === "'" && !inD) inS = !inS;
    else if (c === '"' && !inS) inD = !inD;
  }
  return inS || inD;
}

/** Join continuation lines: trailing `\` splices, quotes keep the newline. */
export function spliceContinuation(buffer: string, next: string): string {
  if (/\\\s*$/.test(buffer)) return buffer.replace(/\\\s*$/, ' ') + next;
  return `${buffer}\n${next}`;
}
