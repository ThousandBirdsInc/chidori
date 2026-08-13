/**
 * The VM-side harness the docs runner wraps around an example before handing
 * it to the wasm engine. It recreates the full documented `chidori:agent`
 * module surface — `run()`, `defineTool()`, captured `fetch`, prompts with
 * tool loops, `context()`/`conversation()`, signals/receive/alarm, memory,
 * workspace, templates, actors, detached agents, branches, sub-agents,
 * app-data, the DOM runtime, and the `util` helpers — so every executable
 * docs example runs unmodified in the browser sandbox.
 *
 * In-VM helpers (context building, templates, util) are pure JavaScript.
 * Everything durable flows through the journaled effect surface the browser
 * SDK provides (`prompt`, `input`, `tool`, `log`, `http_fetch`, `sleep`,
 * `now`, `random`, `signal`); the page-side half lives in run-host.ts.
 *
 * Every observable step is also emitted as one JSON line on the console
 * (the playground's trick): the panel and the docs VM terminal render their
 * feeds purely from journaled console output, so a restored or offline-
 * replayed run repaints identically with zero live calls.
 *
 * NOTE: the prelude is embedded in a template literal — no backticks and no
 * `${` inside the prelude source. String concatenation only.
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

/** Reserved host-tool names the harness shims route through. */
export const INTERNAL_TOOLS = {
  workspace: '__docs.workspace',
  memory: '__docs.memory',
  actors: '__docs.actors',
  branch: '__docs.branch',
  subagent: '__docs.subagent',
  appData: '__docs.appdata',
} as const;

const HARNESS_PRELUDE = `// ---- docs example harness (site-injected) ----
const __feed = (event) => {
  try { console.log(JSON.stringify(event)); } catch (err) { console.log(JSON.stringify({ k: 'note', text: 'unserializable event: ' + String(err) })); }
};
const __rawPrompt = chidori.prompt;
const __rawInput = chidori.input;
const __rawLog = chidori.log;
const __rawFetch = chidori.fetch;
const __rawTool = chidori.tool;
const __rawSignal = chidori.signal;
const __rawStep = chidori.step;
let __runHandler = null;
let __runOptions = null;
function run(handler, options) { __runHandler = handler; __runOptions = options ?? null; }

// ---- input schema validation: run(handler, { inputSchema }) --------------
// Mirrors the native runtime's INPUT_SCHEMA_SCRIPT semantics: a Standard
// Schema validator's value replaces the input; a plain JSON Schema object is
// checked structurally; failures throw InputValidationError listing every
// issue — before the handler (or any journaled effect) runs.
const __fmtPath = (path) => path.map((p) => (typeof p === 'object' && p !== null ? p.key : p)).join('.');
const __jsTypeOf = (v) => {
  if (v === null) return 'null';
  if (Array.isArray(v)) return 'array';
  const t = typeof v;
  if (t === 'number') return Number.isInteger(v) ? 'integer' : 'number';
  return t;
};
function __checkJsonSchema(schema, value, path, issues) {
  if (schema === true || schema == null) return;
  const where = path || 'input';
  if (schema === false) { issues.push(where + ': schema forbids this value'); return; }
  if (typeof schema !== 'object') return;
  if (schema.const !== undefined && JSON.stringify(value) !== JSON.stringify(schema.const)) { issues.push(where + ': expected const ' + JSON.stringify(schema.const)); return; }
  if (Array.isArray(schema.enum) && !schema.enum.some((e) => JSON.stringify(e) === JSON.stringify(value))) { issues.push(where + ': expected one of ' + schema.enum.map((e) => JSON.stringify(e)).join(', ')); return; }
  if (schema.type) {
    const actual = __jsTypeOf(value);
    const allowed = Array.isArray(schema.type) ? schema.type : [schema.type];
    const ok = allowed.some((t) => t === actual || (t === 'number' && actual === 'integer'));
    if (!ok) { issues.push(where + ': expected ' + allowed.join(' | ') + ', got ' + actual); return; }
  }
  if (typeof value === 'string') {
    if (schema.minLength != null && value.length < schema.minLength) issues.push(where + ': shorter than minLength ' + schema.minLength);
    if (schema.maxLength != null && value.length > schema.maxLength) issues.push(where + ': longer than maxLength ' + schema.maxLength);
    if (schema.pattern != null && !(new RegExp(schema.pattern)).test(value)) issues.push(where + ': does not match pattern ' + schema.pattern);
  }
  if (typeof value === 'number') {
    if (schema.minimum != null && value < schema.minimum) issues.push(where + ': below minimum ' + schema.minimum);
    if (schema.maximum != null && value > schema.maximum) issues.push(where + ': above maximum ' + schema.maximum);
  }
  if (Array.isArray(value)) {
    if (schema.minItems != null && value.length < schema.minItems) issues.push(where + ': fewer than minItems ' + schema.minItems);
    if (schema.maxItems != null && value.length > schema.maxItems) issues.push(where + ': more than maxItems ' + schema.maxItems);
    if (schema.items) value.forEach((v, i) => __checkJsonSchema(schema.items, v, where + '[' + i + ']', issues));
  }
  if (value !== null && typeof value === 'object' && !Array.isArray(value)) {
    if (Array.isArray(schema.required)) { for (const key of schema.required) { if (!(key in value)) issues.push(where + '.' + key + ': required'); } }
    if (schema.properties) { for (const key of Object.keys(schema.properties)) { if (key in value) __checkJsonSchema(schema.properties[key], value[key], path ? path + '.' + key : key, issues); } }
    if (schema.additionalProperties === false) {
      const props = schema.properties || {};
      for (const key of Object.keys(value)) { if (!(key in props)) issues.push(where + '.' + key + ': unexpected property'); }
    }
  }
}
async function __validateInput(schema, input) {
  const NL = String.fromCharCode(10);
  const fail = (lines) => {
    const err = new Error('invalid input:' + NL + lines.map((l) => '  - ' + l).join(NL));
    err.name = 'InputValidationError';
    throw err;
  };
  if (schema && typeof schema === 'object' && schema['~standard'] && typeof schema['~standard'].validate === 'function') {
    let result = schema['~standard'].validate(input);
    if (result && typeof result.then === 'function') result = await result;
    if (result.issues) fail(result.issues.map((i) => (i.path && i.path.length ? i.message + ' (at ' + __fmtPath(i.path) + ')' : i.message)));
    return result.value;
  }
  const issues = [];
  __checkJsonSchema(schema, input, '', issues);
  if (issues.length) fail(issues);
  return input;
}

// ---- saga compensations: chidori.compensation.register -------------------
// Registration journals one record (via the log effect, so restored and
// replayed runs repaint identically) and performs nothing; the armed list is
// reported newest-first if the run fails — exactly what a native
// "chidori rollback <run_id>" would execute.
const __compensations = [];
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
chidori.step = (label, fn) => __rawStep(typeof fn === 'function' ? fn : label);
chidori.mark = async (label, data) => {
  __feed({ k: 'op', op: 'mark', label: String(label), data: data === undefined ? null : data });
  return __rawLog('mark:' + String(label), data === undefined ? null : data);
};
chidori.compensation = {
  register: async (name, agent, input) => {
    const entry = { name: String(name), agent: String(agent), input: input === undefined ? null : input };
    await __rawLog('compensation:' + entry.name, entry);
    __compensations.push(entry);
    __feed({ k: 'op', op: 'chidori.compensation.register', label: entry.name + ' \\u2192 ' + entry.agent, data: entry.input });
    return { registered: true };
  },
};

// ---- signals, receive, alarms -------------------------------------------
// All ride the journaled signal effect; the page host implements delivery
// (interactive in the panel/terminal, mailbox-fed for actor messages).
const __signalCall = (names, opts) => __rawSignal(Array.isArray(names) ? names : [String(names)], opts);
chidori.signal = async (nameOrNames, opts) => {
  const names = Array.isArray(nameOrNames) ? nameOrNames.map(String) : [String(nameOrNames)];
  __feed({ k: 'signal', phase: 'waiting', names, timeoutMs: (opts && opts.timeoutMs) ?? null });
  const r = await __signalCall(names, { mode: 'signal', timeoutMs: (opts && opts.timeoutMs) ?? null });
  __feed({ k: 'signal', phase: r && r.timedOut ? 'timeout' : 'received', names, result: r });
  return r;
};
chidori.pollSignal = async (name) => {
  const r = await __signalCall([String(name)], { mode: 'poll' });
  __feed({ k: 'signal', phase: r ? 'received' : 'poll-empty', names: [String(name)], result: r });
  return r;
};
chidori.alarm = async (ms) => {
  __feed({ k: 'op', op: 'alarm', label: String(ms) + ' ms', data: null });
  return __signalCall(['__alarm__'], { mode: 'alarm', ms: Number(ms) });
};
chidori.receive = async (nameOrNames, opts) => {
  const names = Array.isArray(nameOrNames) ? nameOrNames.map(String) : [String(nameOrNames)];
  __feed({ k: 'signal', phase: 'receiving', names, timeoutMs: (opts && opts.timeoutMs) ?? null });
  const r = await __signalCall(names, { mode: 'receive', timeoutMs: (opts && opts.timeoutMs) ?? null });
  __feed({ k: 'signal', phase: r && r.timedOut ? 'timeout' : 'received', names, result: r });
  return r;
};

// ---- state: memory, workspace, app data ---------------------------------
const __hostOp = (tool, payload) => __rawTool(tool, payload);
chidori.memory = {
  set: async (key, value) => { await __hostOp('${INTERNAL_TOOLS.memory}', { action: 'set', key: String(key), value: value ?? null }); __feed({ k: 'op', op: 'memory.set', label: String(key), data: value ?? null }); },
  get: async (key) => { const r = await __hostOp('${INTERNAL_TOOLS.memory}', { action: 'get', key: String(key) }); __feed({ k: 'op', op: 'memory.get', label: String(key), data: r }); return r; },
  list: async (opts) => __hostOp('${INTERNAL_TOOLS.memory}', { action: 'list', prefix: (opts && opts.prefix) ?? null }),
  delete: async (key) => { const r = await __hostOp('${INTERNAL_TOOLS.memory}', { action: 'delete', key: String(key) }); __feed({ k: 'op', op: 'memory.delete', label: String(key), data: r }); return r; },
  clear: async () => { await __hostOp('${INTERNAL_TOOLS.memory}', { action: 'clear' }); __feed({ k: 'op', op: 'memory.clear', label: '', data: null }); },
};
const __wsCall = async (action, payload) => __hostOp('${INTERNAL_TOOLS.workspace}', Object.assign({ action }, payload));
chidori.workspace = {
  list: async (opts) => __wsCall('list', { options: opts ?? null }),
  read: async (path) => {
    const r = await __wsCall('read', { path: String(path) });
    if (r && r.__missing) throw new Error('workspace: no such entry: ' + String(path));
    return r;
  },
  write: async (path, content, opts) => {
    const entry = await __wsCall('write', { path: String(path), content: String(content), options: opts ?? null });
    __feed({ k: 'op', op: 'workspace.write', label: String(path), data: entry });
    return entry;
  },
  delete: async (path, reason) => {
    const r = await __wsCall('delete', { path: String(path), reason: reason === undefined ? null : String(reason) });
    __feed({ k: 'op', op: 'workspace.delete', label: String(path), data: r });
    return r;
  },
  manifest: async () => __wsCall('manifest', {}),
};
chidori.workspace.remove = chidori.workspace.delete;
chidori.appData = {
  write: async (sql, params) => {
    const r = await __hostOp('${INTERNAL_TOOLS.appData}', { op: 'write', sql: String(sql), params: params ?? [] });
    __feed({ k: 'op', op: 'appData.write', label: String(sql), data: r });
    return r;
  },
  query: async (sql, params) => {
    const r = await __hostOp('${INTERNAL_TOOLS.appData}', { op: 'query', sql: String(sql), params: params ?? [] });
    __feed({ k: 'op', op: 'appData.query', label: String(sql), data: r });
    return r;
  },
};

// ---- util helpers (pure in-VM control flow, never journaled) ------------
chidori.util = {
  parallel: async (fns, opts) => {
    const list = Array.from(fns);
    const limit = Math.max(1, Number((opts && opts.concurrency) ?? list.length) || list.length);
    const results = new Array(list.length);
    let next = 0;
    const lane = async () => {
      for (;;) {
        const i = next++;
        if (i >= list.length) return;
        results[i] = await list[i]();
      }
    };
    await Promise.all(Array.from({ length: Math.min(limit, list.length) }, lane));
    return results;
  },
  retry: async (fn, opts) => {
    const attempts = Math.max(1, Number((opts && opts.attempts) ?? 3));
    let lastErr;
    for (let i = 0; i < attempts; i++) {
      try { return await fn(); } catch (err) { lastErr = err; }
    }
    throw lastErr;
  },
  tryCall: async (fn) => {
    try { return { ok: true, value: await fn() }; }
    catch (err) { return { ok: false, error: String(err && err.message ? err.message : err) }; }
  },
};

// ---- the prompt core: plain calls, tool loops, structured turns ---------
const __TOOL_SPECS = {
  hn_search: { description: 'Search Hacker News stories via the Algolia API.', parameters: { type: 'object', properties: { query: { type: 'string' }, sortBy: { type: 'string' } }, required: ['query'] } },
  hn_thread: { description: 'Fetch a Hacker News story and its top comments by objectID.', parameters: { type: 'object', properties: { objectID: { type: 'string' } }, required: ['objectID'] } },
  docs_search: { description: 'Search the chidori docs.', parameters: { type: 'object', properties: { query: { type: 'string' } }, required: ['query'] } },
  search_docs: { description: 'Search the chidori docs.', parameters: { type: 'object', properties: { query: { type: 'string' } }, required: ['query'] } },
  calculate: { description: 'Evaluate an arithmetic expression.', parameters: { type: 'object', properties: { expression: { type: 'string' } }, required: ['expression'] } },
};
const __toolSpec = (t) => {
  if (t && t.__tool) return { name: t.name, description: t.description, parameters: t.parameters };
  const name = String(t);
  const known = __TOOL_SPECS[name];
  return { name, description: (known && known.description) || 'Registered host tool ' + name, parameters: (known && known.parameters) || { type: 'object', properties: {} } };
};
const __runVmTool = async (tools, call) => {
  const handle = tools.find((t) => t && t.__tool && t.name === call.name);
  let result;
  try {
    result = handle ? await handle.run(call.args ?? {}) : await __rawTool(String(call.name), call.args ?? {});
  } catch (err) {
    result = { error: String(err && err.message ? err.message : err) };
  }
  if (result === undefined) result = null;
  __feed({ k: 'tool', name: String(call.name), args: call.args ?? {}, result });
  return result;
};
const __passthrough = (o) => ({
  system: o.system ?? null,
  maxTokens: o.maxTokens ?? null,
  temperature: typeof o.temperature === 'number' ? o.temperature : null,
  kind: o.type ?? null,
  model: o.model ?? null,
  format: o.format ?? null,
});
// One provider hop over an explicit message list (the tool-loop protocol).
const __hop = async (messages, specs, opts) => {
  const raw = await __rawPrompt(JSON.stringify({ messages, tools: specs }), Object.assign(__passthrough(opts), { protocol: '${TOOL_LOOP_PROTOCOL}' }));
  try { return JSON.parse(String(raw)); } catch (err) { return { reply: String(raw) }; }
};
const __parseJsonReply = (text, strict) => {
  let body = String(text).trim();
  // Tolerate a single wrapping markdown fence around the JSON.
  if (body.startsWith('\\u0060\\u0060\\u0060')) {
    body = body.replace(/^\\u0060\\u0060\\u0060[a-zA-Z]*\\s*/, '').replace(/\\u0060\\u0060\\u0060\\s*$/, '');
  }
  try { return JSON.parse(body); }
  catch (err) {
    if (strict === false) return String(text);
    throw new Error('prompt format:"json": reply is not valid JSON: ' + body.slice(0, 120));
  }
};
// The provider tool-use loop, run from inside the VM: each hop is one
// journaled prompt effect; defineTool bodies execute right here, so their
// closures (and their captured fetch) work exactly as documented. Registered
// host tools named as strings dispatch through the journaled tool effect.
const __toolLoop = async (messages, tools, opts) => {
  const specs = tools.map(__toolSpec);
  const maxTurns = Math.max(1, Number(opts.maxTurns ?? 6));
  const work = messages.slice();
  let turns = 0;
  for (let turn = 0; turn < maxTurns; turn++) {
    const decision = await __hop(work, specs, opts);
    const calls = Array.isArray(decision.toolCalls) ? decision.toolCalls : [];
    if (calls.length === 0) return { reply: String(decision.reply ?? ''), turns, messages: work };
    turns++;
    work.push({
      role: 'assistant',
      content: typeof decision.content === 'string' ? decision.content : '',
      tool_calls: calls.map((c) => ({ id: String(c.id), type: 'function', function: { name: String(c.name), arguments: JSON.stringify(c.args ?? {}) } })),
    });
    for (const c of calls) {
      const result = await __runVmTool(tools, c);
      work.push({ role: 'tool', tool_call_id: String(c.id), content: JSON.stringify(result) });
    }
  }
  const bail = '(tool-turn limit of ' + maxTurns + ' reached without a final reply)';
  return { reply: bail, turns, messages: work };
};
chidori.prompt = async (text, opts) => {
  const o = opts ?? {};
  const tools = Array.isArray(o.tools) ? o.tools.filter((t) => t) : [];
  let reply;
  let toolTurns = 0;
  if (tools.length === 0) {
    reply = String(await __rawPrompt(String(text), __passthrough(o)));
  } else {
    const out = await __toolLoop([{ role: 'user', content: String(text) }], tools, o);
    reply = out.reply;
    toolTurns = out.turns;
  }
  __feed(Object.assign({ k: 'prompt', text: String(text), reply }, toolTurns ? { toolTurns } : {}));
  if (o.format === 'json') return __parseJsonReply(reply, o.strict);
  return reply;
};

// ---- context(): the immutable turn-structured prompt builder ------------
const __fnv = (s) => {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) { h ^= s.charCodeAt(i); h = Math.imul(h, 16777619); }
  return (h >>> 0).toString(36);
};
const __ctxMessages = (segs) => {
  const sys = [];
  const msgs = [];
  for (const s of segs) {
    if (s.kind === 'system') sys.push(s.text);
    else if (s.kind === 'doc') sys.push('<document label=' + JSON.stringify(s.label) + '>\\n' + s.text + '\\n</document>');
    else if (s.kind === 'user') msgs.push({ role: 'user', content: s.text });
    else if (s.kind === 'assistant') msgs.push(Object.assign({ role: 'assistant', content: s.text }, s.toolCalls && s.toolCalls.length ? { tool_calls: s.toolCalls.map((c) => ({ id: c.id, type: 'function', function: { name: c.name, arguments: JSON.stringify(c.input ?? {}) } })) } : {}));
    else if (s.kind === 'toolResult') msgs.push({ role: 'tool', tool_call_id: s.id, content: s.content });
  }
  return { system: sys.join('\\n\\n'), msgs };
};
function __makeContext(segs, toolNames) {
  const withSeg = (s) => __makeContext(segs.concat([s]), toolNames);
  const ctx = {
    __context: true,
    __segs: segs,
    system: (text) => withSeg({ kind: 'system', text: String(text) }),
    doc: (label, text) => withSeg({ kind: 'doc', label: String(label), text: String(text) }),
    user: (text) => withSeg({ kind: 'user', text: String(text) }),
    assistant: (text) => withSeg({ kind: 'assistant', text: String(text) }),
    toolResult: (id, content, isError) => withSeg({ kind: 'toolResult', id: String(id), content: String(content), isError: !!isError }),
    cacheBreakpoint: (ttl) => withSeg({ kind: 'breakpoint', ttl: ttl ?? '5m' }),
    tools: (names) => __makeContext(segs.slice(), Array.from(names).map(String)),
    digest: () => __fnv(JSON.stringify([segs, toolNames])),
    estimateTokens: () => Math.ceil(JSON.stringify(segs).length / 4),
    prompt: async (opts) => {
      const o = opts ?? {};
      const { system, msgs } = __ctxMessages(segs);
      const merged = Object.assign({}, o, system ? { system } : {});
      const out = await __toolLoop(msgs, toolNames.slice(), merged);
      __feed(Object.assign({ k: 'prompt', text: '(context: ' + msgs.length + ' turns)', reply: out.reply }, out.turns ? { toolTurns: out.turns } : {}));
      return { text: out.reply, context: withSeg({ kind: 'assistant', text: out.reply }) };
    },
    respond: async (opts) => {
      const o = opts ?? {};
      const { system, msgs } = __ctxMessages(segs);
      const merged = Object.assign({}, o, system ? { system } : {});
      const decision = await __hop(msgs, toolNames.map(__toolSpec), merged);
      const calls = Array.isArray(decision.toolCalls) ? decision.toolCalls : [];
      const content = String(calls.length ? (decision.content ?? '') : (decision.reply ?? decision.content ?? ''));
      const toolCalls = calls.map((c) => ({ id: String(c.id), name: String(c.name), input: c.args ?? {} }));
      const response = {
        content,
        text: content,
        toolCalls,
        blocks: (content ? [{ type: 'text', text: content }] : []).concat(toolCalls.map((c) => ({ type: 'tool_use', id: c.id, name: c.name, input: c.input }))),
        stopReason: toolCalls.length ? 'tool_use' : 'end_turn',
      };
      __feed({ k: 'prompt', text: '(context turn: ' + msgs.length + ' messages)', reply: toolCalls.length ? '(requested ' + toolCalls.length + ' tool call' + (toolCalls.length === 1 ? '' : 's') + ')' : content });
      return { response, context: withSeg({ kind: 'assistant', text: content, toolCalls }) };
    },
    compact: async (opts) => {
      const o = opts ?? {};
      if (o.budgetTokens && ctx.estimateTokens() <= Number(o.budgetTokens)) return ctx;
      const head = [];
      const turns = [];
      for (const s of segs) {
        if (s.kind === 'system' || s.kind === 'doc' || s.kind === 'breakpoint') head.push(s);
        else turns.push(s);
      }
      const keep = Math.max(0, Number(o.keepTurns ?? 2)) * 2;
      if (turns.length <= keep) return ctx;
      const old = turns.slice(0, turns.length - keep);
      const transcript = old.map((s) => (s.kind === 'toolResult' ? 'tool: ' + s.content : s.kind + ': ' + (s.text ?? ''))).join('\\n');
      const summary = String(await __rawPrompt('Summarize this conversation faithfully and briefly, keeping every fact needed to continue it:\\n\\n' + transcript, { system: o.instructions ?? null, maxTokens: o.maxTokens ?? 400, temperature: null, kind: 'compact', model: o.model ?? null }));
      __feed({ k: 'op', op: 'context.compact', label: old.length + ' turns folded into a summary segment', data: null });
      return __makeContext(head.concat([{ kind: 'breakpoint', ttl: o.ttl ?? '5m' }, { kind: 'assistant', text: '(conversation so far, summarized) ' + summary }], turns.slice(turns.length - keep)), toolNames);
    },
  };
  return ctx;
}
chidori.context = (seed) => {
  let ctx = __makeContext([], []);
  if (seed && seed.system) ctx = ctx.system(seed.system);
  if (seed && seed.tools) ctx = ctx.tools(seed.tools);
  return ctx;
};

// ---- conversation(): the stateful chat wrapper --------------------------
chidori.conversation = (options) => {
  const o = options ?? {};
  const defaults = {};
  for (const key of ['type', 'model', 'maxTokens', 'temperature', 'cache']) if (o[key] !== undefined) defaults[key] = o[key];
  const tools = Array.isArray(o.tools) ? o.tools : [];
  let ctx = chidori.context();
  if (o.system) ctx = ctx.system(o.system);
  if (tools.length) ctx = ctx.cacheBreakpoint(o.cacheTtl ?? '5m');
  const turns = [];
  const maybeCompact = async () => { if (o.compact) ctx = await ctx.compact(o.compact); };
  const chat = {
    get context() { return ctx; },
    get length() { return turns.filter((t) => t.role === 'assistant').length; },
    history: () => turns.map((t) => ({ role: t.role, text: t.text })),
    // Each say() is one durable prompt over the whole transcript; the
    // conversation's defineTool handles ride the in-VM tool loop.
    say: async (message, opts) => {
      await maybeCompact();
      ctx = ctx.user(String(message));
      turns.push({ role: 'user', text: String(message) });
      const merged = Object.assign({}, defaults, opts ?? {});
      const { system, msgs } = __ctxMessages(ctx.__segs);
      const out = await __toolLoop(msgs, tools, Object.assign({}, merged, system ? { system } : {}));
      __feed(Object.assign({ k: 'prompt', text: String(message), reply: out.reply }, out.turns ? { toolTurns: out.turns } : {}));
      ctx = ctx.assistant(out.reply);
      turns.push({ role: 'assistant', text: out.reply });
      return out.reply;
    },
    respond: async (message, opts) => {
      await maybeCompact();
      ctx = ctx.user(String(message));
      turns.push({ role: 'user', text: String(message) });
      const { response, context } = await ctx.respond(Object.assign({}, defaults, opts ?? {}));
      ctx = context;
      turns.push({ role: 'assistant', text: response.content });
      return response;
    },
    loop: async (opts) => {
      const lo = opts ?? {};
      const exits = Array.isArray(lo.exit) ? lo.exit : ['exit', 'quit'];
      const maxTurns = Number(lo.maxTurns ?? 40);
      for (let i = 0; i < maxTurns; i++) {
        const message = String(await chidori.input(String(lo.prompt ?? '>'), lo.inputOptions ?? null));
        if (exits.includes(message.trim().toLowerCase())) break;
        if (lo.skipEmpty !== false && message.trim() === '') continue;
        const text = lo.turn ? await lo.turn(message, chat) : await chat.say(message);
        if (lo.onReply) await lo.onReply(text);
        if (lo.until && (await lo.until({ role: 'assistant', text }))) break;
      }
      return chat.history();
    },
  };
  return chat;
};

// ---- templates: a mini-Jinja (inline strings and workspace files) -------
const __tplValue = (path, vars, soft) => {
  const parts = path.split('.');
  let v = vars;
  for (const p of parts) {
    if (v === undefined || v === null) { if (soft) return undefined; throw new Error('template: undefined variable: ' + path); }
    v = v[p];
  }
  return v;
};
const __tplExpr = (expr, vars, soft) => {
  expr = expr.trim();
  const bin = expr.match(/^(.+?)\\s*(==|!=|>=|<=|>|<)\\s*(.+)$/);
  if (bin) {
    const a = __tplExpr(bin[1], vars, true);
    const b = __tplExpr(bin[3], vars, true);
    switch (bin[2]) {
      case '==': return a === b; case '!=': return a !== b;
      case '>=': return a >= b; case '<=': return a <= b;
      case '>': return a > b; default: return a < b;
    }
  }
  if (expr.startsWith('not ')) return !__tplExpr(expr.slice(4), vars, true);
  if (/^".*"$/.test(expr) || /^'.*'$/.test(expr)) return expr.slice(1, -1);
  if (/^-?[0-9.]+$/.test(expr)) return Number(expr);
  if (expr === 'true') return true;
  if (expr === 'false') return false;
  // filters: expr | filter(arg)
  const parts = expr.split('|').map((p) => p.trim());
  let value = __tplValue(parts[0], vars, soft || parts.length > 1);
  for (const f of parts.slice(1)) {
    const m = f.match(/^(\\w+)(?:\\((.*)\\))?$/);
    if (!m) throw new Error('template: bad filter: ' + f);
    const arg = m[2] !== undefined && m[2] !== '' ? __tplExpr(m[2], vars, true) : undefined;
    switch (m[1]) {
      case 'upper': value = String(value).toUpperCase(); break;
      case 'lower': value = String(value).toLowerCase(); break;
      case 'title': value = String(value).replace(/\\b\\w/g, (c) => c.toUpperCase()); break;
      case 'trim': value = String(value).trim(); break;
      case 'length': value = value == null ? 0 : (value.length ?? Object.keys(value).length); break;
      case 'join': value = Array.from(value ?? []).join(String(arg ?? ', ')); break;
      case 'first': value = Array.from(value ?? [])[0]; break;
      case 'last': { const a = Array.from(value ?? []); value = a[a.length - 1]; break; }
      case 'default': if (value === undefined || value === null) value = arg; break;
      default: throw new Error('template: unknown filter: ' + m[1]);
    }
  }
  return value;
};
const __tplRender = async (src, vars, dir) => {
  const tokens = String(src).split(/(\\{\\{[\\s\\S]*?\\}\\}|\\{%[\\s\\S]*?%\\})/);
  let i = 0;
  const renderBlock = async (stopTags) => {
    let out = '';
    while (i < tokens.length) {
      const tok = tokens[i];
      if (tok.startsWith('{%')) {
        const tag = tok.slice(2, -2).trim();
        const word = tag.split(/\\s+/)[0];
        if (stopTags.includes(word)) return { out, tag };
        i++;
        if (word === 'if') {
          let live = !!__tplExpr(tag.slice(3), vars, true);
          let taken = live;
          let branch = await renderBlock(['elif', 'else', 'endif']);
          if (live) out += branch.out;
          while (branch.tag && branch.tag.split(/\\s+/)[0] !== 'endif') {
            const btag = branch.tag;
            i++;
            const isElif = btag.startsWith('elif');
            live = !taken && (isElif ? !!__tplExpr(btag.slice(5), vars, true) : true);
            if (live) taken = true;
            branch = await renderBlock(['elif', 'else', 'endif']);
            if (live) out += branch.out;
          }
          i++; // consume endif
        } else if (word === 'for') {
          const m = tag.match(/^for\\s+(\\w+)\\s+in\\s+(.+)$/);
          if (!m) throw new Error('template: bad for tag: ' + tag);
          const items = __tplExpr(m[2], vars, false);
          if (items === undefined || items === null || typeof items[Symbol.iterator] !== 'function') {
            throw new Error('template: cannot iterate undefined: ' + m[2]);
          }
          const bodyStart = i;
          const list = Array.from(items);
          if (list.length === 0) {
            await renderBlock(['endfor']);
          } else {
            for (let idx = 0; idx < list.length; idx++) {
              i = bodyStart;
              const scoped = Object.assign({}, vars);
              scoped[m[1]] = list[idx];
              scoped.loop = { index: idx + 1, index0: idx, first: idx === 0, last: idx === list.length - 1 };
              const saveVars = Object.assign({}, vars);
              Object.assign(vars, scoped);
              const body = await renderBlock(['endfor']);
              out += body.out;
              for (const k of Object.keys(vars)) delete vars[k];
              Object.assign(vars, saveVars);
            }
          }
          i++; // consume endfor
        } else if (word === 'include') {
          const m = tag.match(/^include\\s+["'](.+)["']$/);
          if (!m) throw new Error('template: bad include tag: ' + tag);
          const rel = (dir ? dir + '/' : '') + m[1];
          const text = await __wsCall('read', { path: rel });
          if (text && text.__missing) throw new Error('template not found: ' + rel);
          out += await __tplRender(text, vars, rel.includes('/') ? rel.slice(0, rel.lastIndexOf('/')) : '');
        } else {
          throw new Error('template: unsupported tag: ' + word);
        }
      } else if (tok.startsWith('{{')) {
        const v = __tplExpr(tok.slice(2, -2), vars, false);
        if (v === undefined) throw new Error('template: undefined variable: ' + tok.slice(2, -2).trim());
        out += typeof v === 'string' ? v : JSON.stringify(v);
        i++;
      } else {
        out += tok;
        i++;
      }
    }
    return { out, tag: null };
  };
  const res = await renderBlock([]);
  // trim_blocks/lstrip_blocks-ish cleanup: drop blank lines left by block tags.
  return res.out.replace(/\\n[ \\t]*\\n[ \\t]*\\n/g, '\\n\\n');
};
chidori.template = async (strOrPath, vars) => {
  const v = Object.assign({}, vars ?? {});
  let src = String(strOrPath);
  let dir = '';
  if (/\\.(jinja|j2)$/.test(src)) {
    const text = await __wsCall('read', { path: src });
    if (text && text.__missing) throw new Error('template not found: ' + src);
    dir = src.includes('/') ? src.slice(0, src.lastIndexOf('/')) : '';
    src = text;
  }
  const rendered = await __tplRender(src, v, dir);
  __feed({ k: 'op', op: 'template', label: String(strOrPath).slice(0, 80), data: rendered.length > 400 ? rendered.slice(0, 400) + '…' : rendered });
  return rendered;
};

// ---- actors, detached agents, branches, sub-agents ----------------------
const __actorsCall = (scope, op, payload) => __hostOp('${INTERNAL_TOOLS.actors}', Object.assign({ scope, op }, payload));
const __makeHandle = (scope, info) => ({
  pid: info.pid,
  name: info.name ?? null,
  runId: info.runId ?? info.pid,
  send: async (name, payload) => {
    const r = await __actorsCall(scope, 'send', { target: info.pid, message: String(name), payload: payload ?? null });
    __feed({ k: 'op', op: scope + '.send', label: (info.name ?? info.pid) + ' ← ' + String(name), data: payload ?? null });
    return r;
  },
  join: async (opts) => {
    const r = await __actorsCall(scope, 'join', { target: info.pid, timeoutMs: (opts && opts.timeoutMs) ?? null });
    __feed({ k: 'op', op: scope + '.join', label: info.name ?? info.pid, data: r });
    return r;
  },
  stop: async () => {
    const r = await __actorsCall(scope, 'stop', { target: info.pid });
    __feed({ k: 'op', op: scope + '.stop', label: info.name ?? info.pid, data: r });
    return r;
  },
  status: async () => __actorsCall(scope, 'status', { target: info.pid }),
});
const __spawner = (scope) => ({
  spawn: async (source, input, opts) => {
    const info = await __actorsCall(scope, 'spawn', { source: String(source), input: input ?? {}, options: opts ?? {} });
    __feed({ k: 'op', op: scope + '.spawn', label: String(source) + ((opts && opts.name) ? ' as ' + opts.name : ''), data: { pid: info.pid, simulated: info.simulated ?? false } });
    return __makeHandle(scope, info);
  },
  send: async (pidOrName, name, payload) => {
    const r = await __actorsCall(scope, 'send', { target: String(pidOrName), message: String(name), payload: payload ?? null });
    __feed({ k: 'op', op: scope + '.send', label: String(pidOrName) + ' ← ' + String(name), data: payload ?? null });
    return r;
  },
  lookup: async (name) => {
    const info = await __actorsCall(scope, 'lookup', { target: String(name) });
    return info ? __makeHandle(scope, info) : null;
  },
});
chidori.actors = __spawner('actors');
chidori.agents = __spawner('agents');
chidori.branch = async (variants, opts) => {
  const list = Array.from(variants ?? []);
  __feed({ k: 'op', op: 'branch', label: list.length + ' variant' + (list.length === 1 ? '' : 's') + ': ' + list.map((v) => v.label ?? v.source).join(', '), data: null });
  const outcomes = await __hostOp('${INTERNAL_TOOLS.branch}', { variants: list, options: opts ?? {} });
  __feed({ k: 'op', op: 'branch.outcomes', label: outcomes.map((o) => (o.label + ': ' + o.status)).join(', '), data: outcomes });
  return outcomes;
};
chidori.callAgent = async (path, input) => {
  __feed({ k: 'op', op: 'callAgent', label: String(path), data: input ?? {} });
  const r = await __hostOp('${INTERNAL_TOOLS.subagent}', { path: String(path), input: input ?? {} });
  __feed({ k: 'op', op: 'callAgent.done', label: String(path), data: r });
  return r;
};

// ---- the DOM runtime shim ----------------------------------------------
let __domMutations = 0;
const __makeEl = (tag) => {
  const el = {
    tagName: String(tag).toUpperCase(),
    attributes: {},
    children: [],
    style: {},
    __text: null,
    appendChild(child) { __domMutations++; el.children.push(child); return child; },
    removeChild(child) { __domMutations++; el.children = el.children.filter((c) => c !== child); return child; },
    setAttribute(name, value) { __domMutations++; el.attributes[String(name)] = String(value); },
    getAttribute(name) { return el.attributes[String(name)] ?? null; },
    addEventListener() {},
    get textContent() { return el.__text ?? el.children.map((c) => c.textContent ?? '').join(''); },
    set textContent(v) { __domMutations++; el.children = []; el.__text = String(v); },
    set innerText(v) { el.textContent = v; },
    get id() { return el.attributes.id ?? ''; },
    set id(v) { el.setAttribute('id', v); },
    get className() { return el.attributes.class ?? ''; },
    set className(v) { el.setAttribute('class', v); },
  };
  return el;
};
const __esc = (s) => String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
const __serializeEl = (el) => {
  if (el.__textNode) return __esc(el.textContent);
  const style = Object.entries(el.style).map(([k, v]) => k.replace(/[A-Z]/g, (c) => '-' + c.toLowerCase()) + ':' + v).join(';');
  const attrs = Object.entries(el.attributes)
    .filter(([k]) => !/^on/i.test(k))
    .map(([k, v]) => ' ' + __esc(k) + '="' + __esc(v) + '"')
    .join('') + (style ? ' style="' + __esc(style) + '"' : '');
  const inner = el.__text !== null ? __esc(el.__text) : el.children.map(__serializeEl).join('');
  return '<' + el.tagName.toLowerCase() + attrs + '>' + inner + '</' + el.tagName.toLowerCase() + '>';
};
globalThis.document = {
  body: __makeEl('body'),
  head: __makeEl('head'),
  createElement: (tag) => __makeEl(tag),
  createTextNode: (text) => { const n = __makeEl('span'); n.__textNode = true; n.__text = String(text); return n; },
  getElementById: (id) => {
    const find = (el) => { if (el.attributes && el.attributes.id === id) return el; for (const c of el.children ?? []) { const hit = find(c); if (hit) return hit; } return null; };
    return find(globalThis.document.body);
  },
  addEventListener() {},
};
globalThis.window = globalThis;
chidori.renderDOM = () => {
  const html = globalThis.document.body.children.map(__serializeEl).join('');
  const ops = __domMutations;
  __domMutations = 0;
  __feed({ k: 'dom', html, ops });
  return { ops, html };
};
`;

/** JSON is almost a JS literal; escape the two exceptions. */
function asJsLiteral(value: unknown): string {
  return JSON.stringify(value ?? {}).replace(/\u2028/g, '\\u2028').replace(/\u2029/g, '\\u2029');
}

/** Drop `import … from "chidori:agent"` — the harness provides that surface. */
export function stripAgentImport(code: string): string {
  return code
    .replace(/^[ \t]*\/\/\/\s*<reference[^\n]*$/gm, '')
    .replace(/^[ \t]*import[^;]*?from\s*["']chidori:agent["'];?[ \t]*$/gm, '');
}

/**
 * Tiny in-VM stand-ins for the npm packages docs examples import (the
 * package-management page). Enough surface for the documented snippets.
 */
const PACKAGE_STUBS: Record<string, string> = {
  zod: `const z = (() => {
  const make = (check) => ({
    __zod: true,
    parse(v) { const r = check(v); if (r !== true) throw new Error('zod: ' + r); return v; },
    safeParse(v) { const r = check(v); return r === true ? { success: true, data: v } : { success: false, error: new Error('zod: ' + r) }; },
    optional() { return make((v) => v === undefined || check(v)); },
    describe() { return this; },
  });
  return {
    string: () => make((v) => typeof v === 'string' || 'expected string'),
    number: () => make((v) => typeof v === 'number' || 'expected number'),
    boolean: () => make((v) => typeof v === 'boolean' || 'expected boolean'),
    array: (inner) => make((v) => Array.isArray(v) || 'expected array'),
    object: (shape) => make((v) => {
      if (typeof v !== 'object' || v === null) return 'expected object';
      for (const key of Object.keys(shape)) {
        const r = shape[key].safeParse ? (shape[key].safeParse(v[key]).success ? true : 'invalid ' + key) : true;
        if (r !== true) return r;
      }
      return true;
    }),
  };
})();`,
  ms: `const ms = (input) => {
  if (typeof input === 'number') return input;
  const m = String(input).trim().match(/^(-?[0-9.]+)\\s*(ms|s|sec|secs|m|min|mins|h|hr|hrs|hours?|d|days?|w|weeks?|y|yrs?|years?)?$/i);
  if (!m) throw new Error('ms: cannot parse ' + input);
  const n = Number(m[1]);
  const unit = (m[2] || 'ms').toLowerCase();
  const table = { ms: 1, s: 1e3, sec: 1e3, secs: 1e3, m: 6e4, min: 6e4, mins: 6e4, h: 36e5, hr: 36e5, hrs: 36e5, hour: 36e5, hours: 36e5, d: 864e5, day: 864e5, days: 864e5, w: 6048e5, week: 6048e5, weeks: 6048e5, y: 315576e5, yr: 315576e5, yrs: 315576e5, year: 315576e5, years: 315576e5 };
  return n * (table[unit] ?? 1);
};`,
};

/**
 * Replace `import x from "pkg"` / `import { a } from "pkg"` lines with the
 * docs VM's package stubs when one exists. Unknown packages are left in
 * place (the block then fails the build-time analysis and stays static).
 */
export function stubPackageImports(code: string): string {
  return code.replace(
    /^[ \t]*import\s+([^;]+?)\s+from\s*["']([a-z0-9@/_-]+)["'];?[ \t]*$/gim,
    (line, _clause: string, pkg: string) => PACKAGE_STUBS[pkg] ?? line,
  );
}

/**
 * Assemble the source handed to BrowserAgent.start: harness + optional
 * ambient declarations (build-time stand-ins for identifiers the docs prose
 * establishes around a fragment) + the example inlined into an async
 * function (legalizing the docs' top-level await and bare `return`) + a
 * driver that invokes any `run()`-registered handler with the reader-
 * provided input.
 */
export function buildHarnessSource(code: string, input: unknown, ambient?: string): string {
  return `${HARNESS_PRELUDE}
async function __example() {
${ambient ? `// ---- ambient context (from the docs prose around this block) ----\n${ambient}\n// ---- the example ----` : ''}
${stubPackageImports(stripAgentImport(code))}
}
async function __main() {
  // A fragment's bare \`return\` value is its output; a program's output is
  // whatever its run()-registered handler returns for the reader's input.
  const fragmentValue = await __example();
  if (fragmentValue !== undefined) __feed({ k: 'result', value: fragmentValue });
  if (__runHandler) {
    let __input = ${asJsLiteral(input)};
    // run(handler, { inputSchema }): validate the reader-provided input
    // before the handler executes — edit the input above to a shape the
    // schema rejects and the run refuses with the issue list.
    if (__runOptions && __runOptions.inputSchema != null) {
      __input = await __validateInput(__runOptions.inputSchema, __input);
      __feed({ k: 'op', op: 'run · inputSchema', label: 'input validated before the handler ran', data: null });
    }
    const output = await __runHandler(__input);
    __feed({ k: 'result', value: output === undefined ? null : output });
  }
  __feed({ k: 'done' });
}
__main().catch((err) => {
  __feed({ k: 'error', text: String(err && err.message ? err.message : err) });
  // The run stopped short: report what a rollback would execute — the
  // registered compensations, newest first (void on a successful run).
  if (__compensations.length) {
    __feed({
      k: 'op',
      op: 'chidori rollback (armed)',
      label: 'compensations run newest-first when this run is rolled back',
      data: __compensations.slice().reverse(),
    });
  }
});
`;
}
