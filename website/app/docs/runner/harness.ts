/**
 * The VM-side harness the docs runner wraps around an example before handing
 * it to the wasm engine. It recreates enough of the native `chidori:agent`
 * module surface — `run()`, `defineTool()`, captured `fetch`, the provider
 * tool-use loop inside `chidori.prompt()` — for the docs' durable-core
 * examples to execute unmodified in the browser sandbox.
 *
 * Every observable step is also emitted as one JSON line on the console
 * (the playground's trick): the panel renders its feed purely from journaled
 * console output, so a restored or offline-replayed run repaints
 * identically with zero live calls.
 */

/**
 * Prompt-effect payloads the shim exchanges with the page's host when a
 * prompt carries tools: the host answers each hop with either tool calls to
 * make or the final reply.
 */
export interface ToolLoopDecision {
  content?: string;
  toolCalls?: { id: string; name: string; args: unknown }[];
  reply?: string;
}

/** Marks a prompt effect as one hop of the shim's tool-use loop. */
export const TOOL_LOOP_PROTOCOL = 'docs-tools-v1';

const HARNESS_PRELUDE = `// ---- docs example harness (site-injected) ----
const __feed = (event) => {
  try { console.log(JSON.stringify(event)); } catch (err) { console.log(JSON.stringify({ k: 'note', text: 'unserializable event: ' + String(err) })); }
};
const __rawPrompt = chidori.prompt;
const __rawInput = chidori.input;
const __rawLog = chidori.log;
const __rawFetch = chidori.fetch;
const __rawTool = chidori.tool;
let __runHandler = null;
function run(handler) { __runHandler = handler; }
function defineTool(def) {
  if (!def || typeof def.name !== 'string' || typeof def.run !== 'function') {
    throw new Error('defineTool needs { name, description, parameters, run }');
  }
  return {
    __tool: true,
    name: def.name,
    description: String(def.description ?? ''),
    parameters: def.parameters ?? { type: 'object', properties: {} },
    run: def.run,
  };
}
// The captured networking surface: a fetch that flows through the journaled
// http_fetch effect, so replays never touch the network.
globalThis.fetch = async (url, init) => {
  const raw = await __rawFetch(String(url), init ?? null);
  __feed({ k: 'fetch', url: String(url), status: raw.status, ok: raw.ok, simulated: !!(raw.json && raw.json.__simulated) });
  return {
    ok: raw.ok,
    status: raw.status,
    text: async () => raw.text,
    json: async () => (raw.json !== null && raw.json !== undefined ? raw.json : JSON.parse(raw.text)),
  };
};
chidori.log = async (message, fields) => {
  __feed({ k: 'log', message: String(message), fields: fields === undefined ? null : fields });
  return __rawLog(String(message), fields);
};
chidori.input = async (message, opts) => {
  const answer = String(await __rawInput(String(message), opts ?? null));
  __feed({ k: 'input', prompt: String(message), answer });
  return answer;
};
chidori.tool = async (name, kwargs) => {
  const result = await __rawTool(String(name), kwargs ?? null);
  __feed({ k: 'tool', name: String(name), args: kwargs ?? {}, result: result === undefined ? null : result });
  return result;
};
// Native signature is step(label, fn); the browser prelude's is step(fn).
const __rawStep = chidori.step;
chidori.step = (label, fn) => __rawStep(typeof fn === 'function' ? fn : label);
chidori.prompt = async (text, opts) => {
  const o = opts ?? {};
  const tools = Array.isArray(o.tools) ? o.tools.filter((t) => t && t.__tool) : [];
  const passthrough = {
    system: o.system ?? null,
    maxTokens: o.maxTokens ?? null,
    temperature: typeof o.temperature === 'number' ? o.temperature : null,
    kind: o.type ?? null,
  };
  if (tools.length === 0) {
    const reply = String(await __rawPrompt(String(text), passthrough));
    __feed({ k: 'prompt', text: String(text), reply });
    return reply;
  }
  // The provider tool-use loop, run from inside the VM: each hop is one
  // journaled prompt effect; tool bodies execute right here, so their
  // closures (and their captured fetch) work exactly as documented.
  const messages = [{ role: 'user', content: String(text) }];
  const specs = tools.map((t) => ({ name: t.name, description: t.description, parameters: t.parameters }));
  const maxTurns = Math.max(1, Number(o.maxTurns ?? 6));
  for (let turn = 0; turn < maxTurns; turn++) {
    const raw = await __rawPrompt(JSON.stringify({ messages, tools: specs }), { ...passthrough, protocol: '${TOOL_LOOP_PROTOCOL}' });
    let decision;
    try { decision = JSON.parse(String(raw)); } catch (err) { decision = { reply: String(raw) }; }
    const calls = Array.isArray(decision.toolCalls) ? decision.toolCalls : [];
    if (calls.length === 0) {
      const reply = String(decision.reply ?? '');
      __feed({ k: 'prompt', text: String(text), reply, toolTurns: turn });
      return reply;
    }
    messages.push({
      role: 'assistant',
      content: typeof decision.content === 'string' ? decision.content : '',
      tool_calls: calls.map((c) => ({
        id: String(c.id),
        type: 'function',
        function: { name: String(c.name), arguments: JSON.stringify(c.args ?? {}) },
      })),
    });
    for (const c of calls) {
      const tool = tools.find((t) => t.name === c.name);
      let result;
      try {
        result = tool ? await tool.run(c.args ?? {}) : { error: 'no such tool: ' + String(c.name) };
      } catch (err) {
        result = { error: String(err) };
      }
      if (result === undefined) result = null;
      __feed({ k: 'tool', name: String(c.name), args: c.args ?? {}, result });
      messages.push({ role: 'tool', tool_call_id: String(c.id), content: JSON.stringify(result) });
    }
  }
  const bail = '(tool-turn limit of ' + maxTurns + ' reached without a final reply)';
  __feed({ k: 'prompt', text: String(text), reply: bail });
  return bail;
};
`;

/** JSON is almost a JS literal; escape the two exceptions. */
function asJsLiteral(value: unknown): string {
  return JSON.stringify(value ?? {}).replace(/\u2028/g, '\\u2028').replace(/\u2029/g, '\\u2029');
}

/** Drop `import … from "chidori:agent"` — the harness provides that surface. */
export function stripAgentImport(code: string): string {
  return code.replace(/^[ \t]*import[^;]*?from\s*["']chidori:agent["'];?[ \t]*$/gm, '');
}

/**
 * Assemble the source handed to BrowserAgent.start: harness + the example
 * inlined into an async function (legalizing the docs' top-level await and
 * bare `return`) + a driver that invokes any `run()`-registered handler with
 * the reader-provided input.
 */
export function buildHarnessSource(code: string, input: unknown): string {
  return `${HARNESS_PRELUDE}
async function __example() {
${stripAgentImport(code)}
}
async function __main() {
  // A fragment's bare \`return\` value is its output; a program's output is
  // whatever its run()-registered handler returns for the reader's input.
  const fragmentValue = await __example();
  if (fragmentValue !== undefined) __feed({ k: 'result', value: fragmentValue });
  if (__runHandler) {
    const output = await __runHandler(${asJsLiteral(input)});
    __feed({ k: 'result', value: output === undefined ? null : output });
  }
  __feed({ k: 'done' });
}
__main().catch((err) => {
  __feed({ k: 'error', text: String(err && err.message ? err.message : err) });
});
`;
}
