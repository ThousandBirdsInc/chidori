'use client';

/**
 * The page-side host for docs example runs: implements the wasm agent's
 * `chidori.prompt` (OpenRouter when the site-wide login is connected, a
 * deterministic offline stand-in otherwise), a small `chidori.tool` table,
 * and a captured-fetch that degrades to a labelled simulated response when
 * the live network call fails (CORS, offline) — the same trick the
 * playground's weather tool uses, so examples still demonstrate the
 * journaling mechanics without a network.
 */

import { type DocsIndex, type Json, searchDocs } from '../../(home)/playground/brain';
import { evaluateExpression } from '../../(home)/playground/tools';
import { getOpenRouterKey, getOpenRouterModel } from '@/lib/openrouter';
import { TOOL_LOOP_PROTOCOL, type ToolLoopDecision } from './harness';

/**
 * What every prompt returns when no key is connected — the browser twin of
 * the CLI's CHIDORI_TEST_LLM_RESPONSE test mode: deterministic, so the
 * journaling/replay mechanics demo identically, just without a real model.
 */
export const OFFLINE_REPLY =
  '(offline test reply — connect OpenRouter in this panel to run the example against a real model)';

/** One rendered step of a run, parsed from a journaled console line. */
export type RunEvent =
  | { k: 'log'; message: string; fields: Json }
  | { k: 'prompt'; text: string; reply: string; toolTurns?: number }
  | { k: 'tool'; name: string; args: Json; result: Json }
  | { k: 'input'; prompt: string; answer: string }
  | { k: 'fetch'; url: string; status: number; ok: boolean; simulated: boolean }
  | { k: 'signal'; phase: 'waiting' | 'receiving' | 'received' | 'timeout' | 'poll-empty'; names: string[]; timeoutMs?: number | null; result?: Json }
  | { k: 'op'; op: string; label: string; data?: Json }
  | { k: 'dom'; html: string; ops: number }
  | { k: 'result'; value: Json }
  | { k: 'error'; text: string }
  | { k: 'done' }
  | { k: 'note'; text: string };

const KINDS = new Set(['log', 'prompt', 'tool', 'input', 'fetch', 'signal', 'op', 'dom', 'result', 'error', 'done', 'note']);

/** Journaled console lines (one JSON event per line) → renderable feed. */
export function parseRunFeed(lines: string[]): RunEvent[] {
  return lines.map((line): RunEvent => {
    try {
      const ev = JSON.parse(line);
      if (ev && typeof ev === 'object' && KINDS.has(ev.k)) return ev as RunEvent;
    } catch {
      /* a console.log from the example itself — show it verbatim */
    }
    return { k: 'note', text: line };
  });
}

/**
 * The offline brain's take on one tool-loop hop: on the first hop it calls
 * the example's first tool once, with arguments derived deterministically
 * from the tool's parameter schema (string properties get the user's text),
 * then replies with the static offline answer. That way a keyless run still
 * demonstrates the documented loop — model asks, runtime executes the tool,
 * result feeds back — and replays byte-identically.
 */
function offlineToolLoopDecision(hop: {
  messages: { role: string; content?: string }[];
  tools: { name: string; description: string; parameters: unknown }[];
}): ToolLoopDecision {
  const calledAlready = hop.messages.some((m) => m.role === 'tool');
  const tool = hop.tools[0];
  if (calledAlready || !tool) return { reply: OFFLINE_REPLY };
  const userText = [...hop.messages].reverse().find((m) => m.role === 'user')?.content ?? '';
  const args: Record<string, unknown> = {};
  const props = (tool.parameters as { properties?: Record<string, { type?: string }> } | undefined)
    ?.properties;
  for (const [name, spec] of Object.entries(props ?? {})) {
    switch (spec?.type) {
      case 'number':
      case 'integer':
        args[name] = 1;
        break;
      case 'boolean':
        args[name] = false;
        break;
      case 'array':
        args[name] = [];
        break;
      case 'object':
        args[name] = {};
        break;
      default:
        args[name] = userText.slice(0, 120);
    }
  }
  return { content: '', toolCalls: [{ id: 'offline_call_1', name: tool.name, args }] };
}

interface OpenRouterMessage {
  content?: string | null;
  tool_calls?: { id: string; function?: { name?: string; arguments?: string } }[];
}

async function chatCompletion(apiKey: string, body: Record<string, unknown>): Promise<OpenRouterMessage> {
  const res = await fetch('https://openrouter.ai/api/v1/chat/completions', {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      authorization: `Bearer ${apiKey}`,
      'HTTP-Referer': typeof location !== 'undefined' ? location.origin : 'https://thousandbirdsinc.github.io',
      'X-Title': 'Chidori Docs Examples',
    },
    body: JSON.stringify(body),
  });
  if (!res.ok) throw new Error(`openrouter: ${res.status} ${await res.text()}`);
  const json = await res.json();
  const message = json.choices?.[0]?.message;
  if (!message) throw new Error('openrouter returned no message');
  return message as OpenRouterMessage;
}

function systemMessages(opts: Record<string, unknown>): { role: string; content: string }[] {
  return typeof opts.system === 'string' && opts.system ? [{ role: 'system', content: opts.system }] : [];
}

/**
 * Serve one `chidori.prompt` effect. Plain prompts become a single chat
 * completion; tool-loop hops (marked by the harness protocol) get the
 * message history + tool schemas and answer with tool calls or a reply.
 * The model always comes from the site-wide panel setting — docs examples
 * name native-runtime model aliases that OpenRouter wouldn't resolve.
 */
export async function decidePrompt(payload: { text: string; opts?: unknown }): Promise<string> {
  const opts = (payload.opts ?? {}) as Record<string, unknown>;
  const key = getOpenRouterKey();
  if (opts.protocol === TOOL_LOOP_PROTOCOL) {
    const hop = JSON.parse(payload.text) as {
      messages: { role: string; content?: string }[];
      tools: { name: string; description: string; parameters: unknown }[];
    };
    if (!key) return JSON.stringify(offlineToolLoopDecision(hop));
    const message = await chatCompletion(key, {
      model: getOpenRouterModel(),
      messages: [...systemMessages(opts), ...hop.messages],
      tools: hop.tools.map((t) => ({ type: 'function', function: t })),
    });
    const calls = message.tool_calls ?? [];
    if (calls.length > 0) {
      const decision: ToolLoopDecision = {
        content: message.content ?? '',
        toolCalls: calls.map((c, i) => {
          let args: unknown = {};
          try {
            args = JSON.parse(c.function?.arguments || '{}');
          } catch {
            /* malformed arguments — hand the tool an empty object */
          }
          return { id: c.id || `call_${i}`, name: c.function?.name ?? '', args };
        }),
      };
      return JSON.stringify(decision);
    }
    return JSON.stringify({ reply: String(message.content ?? '') } satisfies ToolLoopDecision);
  }
  // Honor format:"json" in the offline stand-in: the harness parses the
  // reply, so hand it a valid JSON string literal instead of bare prose.
  if (!key) return opts.format === 'json' ? JSON.stringify(OFFLINE_REPLY) : OFFLINE_REPLY;
  const message = await chatCompletion(key, {
    model: getOpenRouterModel(),
    messages: [...systemMessages(opts), { role: 'user', content: payload.text }],
    ...(opts.maxTokens ? { max_tokens: Number(opts.maxTokens) } : {}),
    ...(typeof opts.temperature === 'number' ? { temperature: opts.temperature } : {}),
  });
  return String(message.content ?? '');
}

const asObj = (kwargs: Json): Record<string, Json> =>
  kwargs && typeof kwargs === 'object' && !Array.isArray(kwargs) ? kwargs : {};

/**
 * The registry behind `chidori.tool()` calls in docs examples: docs search
 * (both spellings the docs use), the playground's deterministic calculator,
 * and the Hacker News research tools the usability-review walkthroughs use
 * (Algolia's API is CORS-enabled, so they work live from the browser; when
 * the network is unavailable they degrade to a labelled simulated result).
 */
export function makeDocsTools(
  getIndex: () => DocsIndex | null,
): Record<string, (kwargs: Json) => Json | Promise<Json>> {
  const search = (kwargs: Json): Json => {
    const query = String(asObj(kwargs).query ?? '');
    const index = getIndex();
    return {
      query,
      hits: searchDocs(index, query, 4) as unknown as Json,
      ...(index ? {} : { note: 'docs index not loaded' }),
    };
  };
  const hnFetch = async (url: string): Promise<Json> => {
    const res = await fetchWithSimulatedFallback(url);
    return (await res.json()) as Json;
  };
  return {
    docs_search: search,
    search_docs: search,
    calculate: (kwargs) => {
      const expression = String(asObj(kwargs).expression ?? '');
      return { expression, value: evaluateExpression(expression) };
    },
    hn_search: async (kwargs) => {
      const args = asObj(kwargs);
      const endpoint = args.sortBy === 'date' ? 'search_by_date' : 'search';
      const data = asObj(
        await hnFetch(
          `https://hn.algolia.com/api/v1/${endpoint}?tags=story&hitsPerPage=8&query=${encodeURIComponent(String(args.query ?? ''))}`,
        ),
      );
      if (data.__simulated) return { query: args.query ?? '', hits: [], note: 'offline — simulated empty result' } as Json;
      const hits = (Array.isArray(data.hits) ? data.hits : []).map((h) => {
        const hit = asObj(h);
        return {
          objectID: hit.objectID ?? null,
          title: hit.title ?? null,
          url: hit.url ?? null,
          points: hit.points ?? null,
          numComments: hit.num_comments ?? null,
          createdAt: hit.created_at ?? null,
        };
      });
      return { query: args.query ?? '', hits } as Json;
    },
    hn_thread: async (kwargs) => {
      const args = asObj(kwargs);
      const data = asObj(await hnFetch(`https://hn.algolia.com/api/v1/items/${encodeURIComponent(String(args.objectID ?? ''))}`));
      if (data.__simulated) return { objectID: args.objectID ?? '', note: 'offline — simulated empty thread', comments: [] } as Json;
      const strip = (html: unknown) => String(html ?? '').replace(/<[^>]+>/g, ' ').replace(/\s+/g, ' ').trim();
      const comments = (Array.isArray(data.children) ? data.children : [])
        .slice(0, 12)
        .map((c) => {
          const comment = asObj(c);
          return { author: comment.author ?? null, text: strip(comment.text).slice(0, 600) };
        })
        .filter((c) => c.text);
      return { objectID: args.objectID ?? '', title: data.title ?? null, points: data.points ?? null, comments } as Json;
    },
  };
}

/**
 * The captured-network implementation handed to the BrowserAgent host: try
 * the real fetch; when it fails (CORS, DNS, offline), answer with a clearly
 * labelled simulated response instead of killing the run, so the journaling
 * story the example demonstrates still plays out. The `__simulated` marker
 * is what the harness surfaces in its fetch feed events.
 */
export async function fetchWithSimulatedFallback(
  input: RequestInfo | URL,
  init?: RequestInit,
): Promise<Response> {
  try {
    return await fetch(input, init);
  } catch (err) {
    return new Response(
      JSON.stringify({
        ok: true,
        __simulated: true,
        note: `live request failed in the browser sandbox (${String(err)}) — this is a simulated response`,
      }),
      { status: 200, headers: { 'content-type': 'application/json' } },
    );
  }
}
