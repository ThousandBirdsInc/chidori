/**
 * The "decide" half of the playground agent: given the agent's transcript,
 * produce one JSON decision — {tool, args} or {reply}.
 *
 * Two interchangeable brains serve `chidori.prompt()`:
 *  - a real model via OpenRouter (grounded in the docs index), or
 *  - a deterministic offline router, so the playground works with no key
 *    and replays byte-identically.
 */

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

export interface ChatMessage {
  role: 'user' | 'assistant' | 'tool';
  content: string;
}

export interface Decision {
  tool?: string;
  args?: Json;
  reply?: string;
}

export type FeedEvent =
  | { kind: 'user'; text: string }
  | { kind: 'assistant'; text: string }
  | { kind: 'tool'; name: string; args?: Json; result?: Json }
  | { kind: 'note'; text: string };

/** Console lines (one JSON event per line) → renderable feed. */
export function parseFeed(lines: string[]): FeedEvent[] {
  return lines.map((line): FeedEvent => {
    try {
      const ev = JSON.parse(line);
      if (ev && (ev.kind === 'user' || ev.kind === 'assistant') && typeof ev.text === 'string') return ev;
      if (ev && ev.kind === 'tool' && typeof ev.name === 'string') return ev;
    } catch {
      /* not an event line */
    }
    return { kind: 'note', text: line };
  });
}

// ---------------------------------------------------------------------------
// Docs index: built by scripts/build-playground-context.mjs into
// public/playground-docs.json; grounds the system prompt and serves the
// agent's `search_docs` tool.

export interface DocSection {
  heading: string;
  text: string;
  /** Lowercased copies, cached once at load time for scoring. */
  lc?: string;
  hlc?: string;
}

export interface DocPage {
  slug: string;
  route: string;
  title: string;
  tlc?: string;
  sections: DocSection[];
}

export interface DocsIndex {
  pages: DocPage[];
}

export interface DocHit {
  title: string;
  heading: string;
  route: string;
  excerpt: string;
}

export function prepareDocsIndex(raw: DocsIndex): DocsIndex {
  for (const page of raw.pages) {
    page.tlc = page.title.toLowerCase();
    for (const s of page.sections) {
      s.lc = s.text.toLowerCase();
      s.hlc = s.heading.toLowerCase();
    }
  }
  return raw;
}

const STOPWORDS = new Set([
  'the', 'and', 'for', 'with', 'that', 'this', 'what', 'how', 'does', 'can',
  'you', 'are', 'was', 'not', 'has', 'have', 'its', 'about', 'from', 'into',
  'when', 'why', 'where', 'which', 'chidori',
]);

function terms(query: string): string[] {
  return (query.toLowerCase().match(/[a-z0-9]{3,}/g) ?? []).filter((t) => !STOPWORDS.has(t));
}

function countOccurrences(haystack: string, needle: string): number {
  let n = 0;
  for (let i = haystack.indexOf(needle); i !== -1 && n < 5; i = haystack.indexOf(needle, i + needle.length)) n++;
  return n;
}

export function searchDocs(index: DocsIndex | null, query: string, k = 4): DocHit[] {
  if (!index) return [];
  const ts = terms(query);
  if (!ts.length) return [];
  const scored: { score: number; hit: DocHit }[] = [];
  for (const page of index.pages) {
    for (const s of page.sections) {
      let score = 0;
      for (const t of ts) {
        score += countOccurrences(s.lc ?? '', t);
        if ((page.tlc ?? '').includes(t)) score += 4;
        if ((s.hlc ?? '').includes(t)) score += 2;
      }
      if (score > 0) {
        scored.push({
          score,
          hit: {
            title: page.title,
            heading: s.heading,
            route: page.route,
            excerpt: s.text.slice(0, 300),
          },
        });
      }
    }
  }
  scored.sort((a, b) => b.score - a.score);
  // At most two sections per page, so one page can't fill every slot.
  const hits: DocHit[] = [];
  const perPage = new Map<string, number>();
  for (const { hit } of scored) {
    const seen = perPage.get(hit.route) ?? 0;
    if (seen >= 2) continue;
    perPage.set(hit.route, seen + 1);
    hits.push(hit);
    if (hits.length >= k) break;
  }
  return hits;
}

// ---------------------------------------------------------------------------
// Shared prompt material

export const OVERVIEW =
  'Chidori is an agent framework where every run is durable, replayable, and ' +
  'resumable by default. Agents are plain async TypeScript (real if/for/try, ' +
  'no graph DSL) executed by a pure-Rust runtime — its own JS engine, GC, and ' +
  'a journaled host-effect boundary. Because every side effect (LLM calls, ' +
  'tools, HTTP, input, timers, randomness) is journaled, a run can suspend at ' +
  'chidori.input(), be saved as one blob, resume later, and replay offline ' +
  'with byte-identical output. The engine also compiles to WebAssembly — ' +
  'this very chat is a chidori agent running in your browser tab.';

const TOOL_SPEC = `Tools you can call (one per decision):
- search_docs {query: string} — full-text search over the chidori documentation; returns titles, links, and excerpts. Call this before answering any question about chidori.
- weather {city: string} — live current conditions + 5-day forecast (Open-Meteo, no key).
- calculate {expression: string} — arithmetic with + - * / % ^ ( ), functions sqrt sin cos tan abs ln log exp round floor ceil, constants pi and e.
- chart {title?: string, kind?: "bar" | "line", series: [{label: string, value: number}, ...]} — renders a chart card in the chat. Provide the data yourself.
- color_palette {mood: string, colors: [{hex: string, name: string} x5]} — renders five swatches. Pick the hex values yourself.
- roll_dice {count?: number, sides?: number} — fair dice, rolled by the host.
- read_source {} — your own source code: the chidori agent program currently running this conversation.
- update_source {find: string, replace: string} or {source: string} — rewrite your own implementation. The find text must occur exactly once in the current source (call read_source first and copy it verbatim). The edit is validated by replaying this conversation's journal against the new code, then hot-swapped in when the turn ends.
- reset_source {} — go back to the original playground source.`;

const SELF_MOD_GUIDE = `Self-modification: you are allowed — encouraged — to rewrite your own program when asked. Call read_source before editing, and prefer small {find, replace} patches over full rewrites. The runtime enforces modify-and-resume: your journal (every past prompt/tool/input call) is replayed against the new code, so an edit is rejected as divergence if it changes calls that already happened — e.g. altering what past turns pushed into the transcript. Changing only emitted output (the emit(...) lines) or future behavior is safe, and past turns will re-render through the new code.`;

export function buildSystemPrompt(index: DocsIndex | null, latestUser: string): string {
  const hits = searchDocs(index, latestUser, 4);
  const context = hits.length
    ? hits
        .map((h) => `[${h.title}${h.heading ? ` — ${h.heading}` : ''}] (${h.route})\n${h.excerpt}`)
        .join('\n\n')
    : '(no matching docs sections for this message)';
  return `You are the Chidori Playground agent — a chidori agent running client-side on the wasm build of the chidori runtime, inside the visitor's browser on the chidori docs site.

About chidori: ${OVERVIEW}

${TOOL_SPEC}

${SELF_MOD_GUIDE}

Protocol — respond with EXACTLY one JSON object and nothing else:
  {"tool": "<name>", "args": {...}}   to call a tool, or
  {"reply": "<text>"}                 to answer the user.
Messages whose content starts with TOOL RESULT are journaled tool outputs from your own earlier calls this turn.

Guidance: ground chidori answers in search_docs results and name the doc pages you drew from; prefer showing over telling (chart, color_palette); keep replies under ~120 words; plain text only.

Docs context retrieved for the current message:
${context}`;
}

// ---------------------------------------------------------------------------
// Real brain: OpenRouter chat completions (CORS-enabled for browser use).

export async function openRouterDecide(cfg: {
  apiKey: string;
  model: string;
  transcript: ChatMessage[];
  index: DocsIndex | null;
}): Promise<Decision> {
  const { apiKey, model, transcript, index } = cfg;
  const latestUser = [...transcript].reverse().find((m) => m.role === 'user')?.content ?? '';
  const messages = [
    { role: 'system', content: buildSystemPrompt(index, latestUser) },
    ...transcript.slice(-30).map((m) => ({
      role: m.role === 'tool' ? 'user' : m.role,
      content: (m.role === 'tool' ? `TOOL RESULT ${m.content}` : m.content).slice(0, 4000),
    })),
  ];
  const res = await fetch('https://openrouter.ai/api/v1/chat/completions', {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      authorization: `Bearer ${apiKey}`,
      'HTTP-Referer': typeof location !== 'undefined' ? location.origin : 'https://thousandbirdsinc.github.io',
      'X-Title': 'Chidori Playground',
    },
    body: JSON.stringify({ model, messages }),
  });
  if (!res.ok) throw new Error(`openrouter: ${res.status} ${await res.text()}`);
  const body = await res.json();
  return extractDecision(String(body.choices?.[0]?.message?.content ?? ''));
}

/** Normalize whatever the model emitted into a protocol decision. */
export function extractDecision(raw: string): Decision {
  let text = raw.trim();
  const fence = /^```[a-z]*\s*([\s\S]*?)\s*```$/.exec(text);
  if (fence) text = fence[1].trim();
  const start = text.indexOf('{');
  const end = text.lastIndexOf('}');
  if (start !== -1 && end > start) {
    try {
      const obj = JSON.parse(text.slice(start, end + 1));
      if (obj && typeof obj === 'object' && (typeof obj.tool === 'string' || typeof obj.reply === 'string')) {
        return obj as Decision;
      }
    } catch {
      /* fall through to plain reply */
    }
  }
  return { reply: raw };
}

// ---------------------------------------------------------------------------
// Offline brain: a deterministic router, so the playground demos every tool
// (and docs Q&A) with no key, and replays are byte-identical.

export function mockDecide(transcript: ChatMessage[], index: DocsIndex | null): Decision {
  let userIdx = -1;
  for (let i = transcript.length - 1; i >= 0; i--) {
    if (transcript[i].role === 'user') {
      userIdx = i;
      break;
    }
  }
  const text = userIdx >= 0 ? transcript[userIdx].content : '';
  const toolMsgs = transcript.slice(userIdx + 1).filter((m) => m.role === 'tool');
  if (toolMsgs.length) {
    try {
      return composeReply(JSON.parse(toolMsgs[toolMsgs.length - 1].content));
    } catch {
      return { reply: 'That tool result confused me — try rephrasing?' };
    }
  }
  return route(text, index);
}

/**
 * The line the offline brain's canned self-edit patches: appending to the
 * *emitted* reply (not the transcript) keeps every already-journaled
 * `chidori.prompt` call byte-identical, so the swap replays cleanly and past
 * turns re-render with the signature.
 */
const EMIT_REPLY_LINE = "emit({ kind: 'assistant', text: reply });";

function route(text: string, index: DocsIndex | null): Decision {
  const t = text.toLowerCase();

  // Self-modification: the agent reading, patching, and resetting its own
  // program. Checked first — "code" and "source" say exactly what is meant.
  if (/\b(your|its|my) (own )?(source|code|implementation)\b|\byourself\b/.test(t)) {
    if (/\b(reset|revert|restore)\b/.test(t) || /\b(original|default)\b/.test(t)) {
      return { tool: 'reset_source', args: {} };
    }
    if (/\b(rewrite|modify|change|edit|update|patch|improve|upgrade)\b/.test(t)) {
      const quoted = /["“”']([^"“”']{1,24})["“”']/.exec(text)?.[1];
      const emoji = /\p{Extended_Pictographic}/u.exec(text)?.[0];
      const sig = (quoted ?? emoji ?? '⚡').replace(/[\\'`]/g, '').trim() || '⚡';
      return {
        tool: 'update_source',
        args: {
          find: EMIT_REPLY_LINE,
          replace: `emit({ kind: 'assistant', text: reply + ' ${sig}' });`,
        },
      };
    }
    return { tool: 'read_source', args: {} };
  }

  const dice = /(\d+)\s*d\s*(\d+)/.exec(t);
  if (dice || /\b(roll|dice)\b/.test(t)) {
    return {
      tool: 'roll_dice',
      args: { count: dice ? Number(dice[1]) : 2, sides: dice ? Number(dice[2]) : 6 },
    };
  }

  if (/\b(weather|forecast|temperature|raining|sunny|snowing)\b/.test(t)) {
    const m = /\b(?:in|for|at)\s+([a-zA-Z][a-zA-Z\s'.-]*)/.exec(text);
    let city = m ? m[1] : text.replace(/[^a-zA-Z\s'.-]/g, ' ');
    city = city
      .replace(/\b(the|weather|forecast|temperature|right|now|today|tomorrow|please|what|whats|is|like)\b/gi, ' ')
      .replace(/\s+/g, ' ')
      .trim();
    return { tool: 'weather', args: { city: city || 'Tokyo' } };
  }

  if (/\b(chart|plot|graph)\b/.test(t)) {
    const kind = /\b(line|trend|over time)\b/.test(t) ? 'line' : 'bar';
    let values: number[];
    let title: string;
    if (/fibonacci|fib\b/.test(t)) {
      const n = Math.min(Number((/\b(\d+)\b/.exec(t) ?? [])[1] ?? 10), 16);
      values = [1, 1];
      while (values.length < n) values.push(values[values.length - 1] + values[values.length - 2]);
      values = values.slice(0, n);
      title = `First ${values.length} Fibonacci numbers`;
    } else if (/\bsquares?\b/.test(t)) {
      const n = Math.min(Number((/\b(\d+)\b/.exec(t) ?? [])[1] ?? 8), 16);
      values = Array.from({ length: n }, (_, i) => (i + 1) * (i + 1));
      title = `Squares 1–${n}`;
    } else {
      values = (text.match(/-?\d+(?:\.\d+)?/g) ?? []).map(Number).slice(0, 24);
      title = 'Your numbers';
    }
    if (!values.length) {
      return { reply: 'Give me numbers to chart — e.g. "chart 3 1 4 1 5 9" or "chart the first 10 fibonacci numbers".' };
    }
    return {
      tool: 'chart',
      args: { title, kind, series: values.map((v, i) => ({ label: String(i + 1), value: v })) },
    };
  }

  if (/\b(palette|colou?rs?|swatch)\b/.test(t)) {
    const mood =
      text
        .replace(/\b(a|an|the|for|of|make|give|me|generate|show|color|colour|colors|colours|palette|swatches?|please)\b/gi, ' ')
        .replace(/[^\w\s-]/g, ' ')
        .replace(/\s+/g, ' ')
        .trim() || 'chidori';
    return { tool: 'color_palette', args: { mood, colors: paletteFor(mood) } };
  }

  const calcM = /(?:what\s+is|calc(?:ulate)?|compute|evaluate)\s+(.+)|^([\d\s+\-*/^%().]+)[?]?$/i.exec(text.trim());
  if (calcM) {
    const expr = (calcM[1] ?? calcM[2] ?? '').replace(/[?.]+$/, '').trim();
    if (/\d/.test(expr) && /^[\d\s+\-*/^%().,a-z]+$/i.test(expr) && /[\d)]\s*[+\-*/^%]|\b(sqrt|sin|cos|tan|log|ln|abs|exp|pi)\b/i.test(expr)) {
      return { tool: 'calculate', args: { expression: expr } };
    }
  }

  if (index && (/\?\s*$/.test(text) || /\b(chidori|agent|replay|durable|journal|resume|suspend|wasm|runtime|effect|host|deploy|cli|actor|sdk|typescript|prompt|memory|browser)\b/.test(t))) {
    return { tool: 'search_docs', args: { query: text } };
  }

  return {
    reply:
      'Offline brain here (connect OpenRouter above for a real model). I can still do a lot deterministically — try:\n' +
      '• "How does offline replay work?" (searches these docs)\n' +
      '• "Weather in Tokyo"\n' +
      '• "Chart the first 10 fibonacci numbers"\n' +
      '• "What is 2^16 / 3?"  • "Roll 3d6"  • "A palette for a storm at dusk"\n' +
      '• "Show me your own source code"  • "Rewrite your code: add a ⚡ to every reply"',
  };
}

function composeReply(toolMsg: { name?: string; result?: Json }): Decision {
  const name = toolMsg.name ?? '';
  const r = (toolMsg.result ?? {}) as Record<string, Json>;
  if (r && typeof r === 'object' && 'error' in r) {
    return { reply: `That ${name} call failed: ${String(r.error)}` };
  }
  switch (name) {
    case 'weather': {
      const cond = r.condition as { label?: string } | undefined;
      return {
        reply:
          `${String(r.city)}: ${String(r.tempC)}°C and ${String(cond?.label ?? 'unknown').toLowerCase()}, ` +
          `wind ${String(r.windKph)} km/h, humidity ${String(r.humidity)}%.` +
          (r.simulated ? ' (Network was unavailable, so this one is simulated.)' : ''),
      };
    }
    case 'search_docs': {
      const hits = (r.hits ?? []) as unknown as DocHit[];
      if (!hits.length) return { reply: 'Nothing in the docs matched — try different words.' };
      const top = hits[0];
      return {
        reply:
          `From “${top.title}”${top.heading ? ` (§ ${top.heading})` : ''}:\n${top.excerpt}\n\n` +
          'The cards above link to the full pages.',
      };
    }
    case 'calculate':
      return { reply: `${String(r.expression)} = ${String(r.value)}` };
    case 'chart':
      return { reply: `Charted ${String(r.points)} points — rendered above.` };
    case 'roll_dice': {
      const rolls = (r.rolls ?? []) as number[];
      return { reply: `🎲 ${rolls.join(' + ')} = ${String(r.total)}` };
    }
    case 'color_palette':
      return { reply: `Five swatches for “${String(r.mood)}” — rendered above.` };
    case 'read_source':
      return {
        reply:
          `That's me — ${String(r.lines)} lines of TypeScript${r.modified ? ' (already rewritten via this chat)' : ''}, ` +
          'and it really is the program running this conversation on the wasm engine. ' +
          'Ask me to change it — e.g. "rewrite your code: add a ⚡ to every reply" — and I\'ll hot-swap myself mid-conversation.',
      };
    case 'update_source':
      return {
        reply:
          `Done — I ${r.mode === 'patch' ? 'patched' : 'rewrote'} my own source. The edit was validated by replaying ` +
          'this conversation\'s journal against the new code; when this reply lands, the page hot-swaps it in. ' +
          'Watch the feed: my past turns re-render through the new implementation.',
      };
    case 'reset_source':
      return {
        reply: r.unchanged
          ? 'I\'m already running my original source — nothing to reset.'
          : 'Back to the original source — the swap lands as this turn ends, and the journal replays against it.',
      };
    default:
      return { reply: `Done: ${JSON.stringify(r).slice(0, 200)}` };
  }
}

// ---------------------------------------------------------------------------
// Small deterministic helpers

export function hashString(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

export function hslToHex(h: number, s: number, l: number): string {
  const ln = l / 100;
  const a = (s * Math.min(ln, 1 - ln)) / 100;
  const f = (n: number) => {
    const k = (n + h / 30) % 12;
    const c = ln - a * Math.max(Math.min(k - 3, 9 - k, 1), -1);
    return Math.round(255 * c)
      .toString(16)
      .padStart(2, '0');
  };
  return `#${f(0)}${f(8)}${f(4)}`;
}

const SWATCH_NAMES = ['base', 'shade', 'accent', 'glow', 'contrast'];

/** Five deterministic swatches derived from the mood string. */
export function paletteFor(mood: string): { hex: string; name: string }[] {
  const seed = hashString(mood.toLowerCase());
  const baseHue = seed % 360;
  return SWATCH_NAMES.map((name, i) => {
    const hue = (baseHue + i * 137.5) % 360;
    const sat = 35 + ((seed >>> (i * 3)) % 45);
    const light = 30 + ((seed >>> (i * 5)) % 45);
    return { hex: hslToHex(hue, sat, light), name };
  });
}
