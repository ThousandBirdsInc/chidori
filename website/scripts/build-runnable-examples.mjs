// Builds two public assets from docs/ + examples/:
//
//   public/runnable-examples.json — an index of every docs code block that
//   can execute in the reader's browser, keyed by content hash. ts/js blocks
//   run on the wasm engine behind the "Run" button (mode program/fragment,
//   with an optional editable input template and optional `ambient` JS that
//   stands in for identifiers the surrounding prose establishes); bash/sh
//   blocks play in the docs VM terminal (mode shell).
//
//   public/vm-seed.json — the docs VM's filesystem: the repo's example
//   agents, agent files reconstructed from docs pages (the file a page's
//   `chidori run foo.ts` refers to is the page's own code block), and small
//   worker/strategy agents for the sources the multi-agent examples spawn.
//
// A ts/js block qualifies when, after dropping its `chidori:agent` import
// (and stubbing the docs' known npm imports), it is syntactically valid,
// touches only the chidori API surface the browser harness implements, and
// every remaining free identifier either is a runner-provided global or has
// an entry in the AMBIENT table below. Reference listings (pseudo-signature
// blocks) fail the syntax check and stay static on purpose.
//
// Runs before `next dev`/`next build` (see package.json); output is
// gitignored like the wasm assets.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import ts from 'typescript';

const here = path.dirname(fileURLToPath(import.meta.url));
const docsDir = path.resolve(here, '../../docs');
const examplesDir = path.resolve(here, '../../examples');
const outIndex = path.resolve(here, '../public/runnable-examples.json');
const outSeed = path.resolve(here, '../public/vm-seed.json');

// Mirror the site's content rules (source.config.ts): every docs/*.md except
// the GitHub-facing README and the posts thread, which are not site pages.
const EXCLUDE = new Set(['README.md', 'posts/harness-engineering-thread.md']);

// The chidori.* surface the browser harness implements (harness.ts).
const SUPPORTED_API = new Set([
  'prompt', 'input', 'tool', 'log', 'step', 'sleep', 'now', 'random', 'fetch',
  'signal', 'pollSignal', 'alarm', 'receive', 'mark', 'compensation',
  'memory', 'workspace', 'appData', 'util', 'template',
  'context', 'conversation',
  'actors', 'agents', 'branch', 'callAgent', 'renderDOM',
]);

// Globals the runner's VM provides: the harness surface (chidori/run/
// defineTool/fetch/document/window), plus engine built-ins.
const ALLOWED_GLOBALS = new Set([
  'chidori', 'run', 'defineTool', 'fetch', 'console', 'document', 'window',
  'JSON', 'Math', 'Object', 'Array', 'String', 'Number', 'Boolean', 'Symbol',
  'Promise', 'Date', 'RegExp', 'Map', 'Set', 'WeakMap', 'WeakSet',
  'Error', 'TypeError', 'RangeError', 'SyntaxError',
  'parseInt', 'parseFloat', 'isNaN', 'isFinite',
  'encodeURIComponent', 'decodeURIComponent', 'encodeURI', 'decodeURI',
  'NaN', 'Infinity', 'undefined', 'globalThis', 'arguments',
]);

// npm packages the harness stubs in-VM (harness.ts stubPackageImports):
// package name → the identifier its stub defines.
const STUB_PACKAGES = { zod: 'z', ms: 'ms' };

/**
 * Stand-ins for identifiers the docs prose establishes around a fragment
 * ("given a `worker` from the previous block…"). Injected ahead of the
 * block inside the harness, and shown to the reader in the panel. Entries
 * may reference the chidori API — they run in the same VM.
 */
const AMBIENT = {
  input: `const input = { document: "Chidori is a durable TypeScript agent runtime: every chidori.* call is journaled live and replayed from the log for $0.", corpus: "(sample corpus) §1 Chidori journals every host call. §2 Replay returns recorded results without re-executing effects. §3 Signals pause runs for outside parties.", question: "What does a replay cost?", questions: ["What is a durable host call?", "How does replay stay deterministic?"], topic: "durable agents", name: "reader", request: "ship the TypeScript runtime", shards: ["shard-a", "shard-b"] };`,
  worker: `const worker = await chidori.actors.spawn("workers/researcher.ts", { topic: "pricing" }, { name: "researcher" });`,
  svc: `const svc = await chidori.agents.spawn("services/inbox-triager.ts", {}, { name: "inbox-triager" });`,
  chat: `const chat = chidori.conversation({ system: "You are a concise, friendly assistant." });`,
  ctx: `let ctx = chidori.context().system("You are concise.").user("What is a durable host call?");`,
  questions: `const questions = ["What is a durable host call?", "How does replay stay deterministic?"];`,
  corpus: `const corpus = "(sample corpus) §1 Chidori journals every host call. §2 Replay returns recorded results without re-executing effects. §3 Signals pause runs for outside parties.";`,
  corpusText: `const corpusText = "(sample corpus) §1 Chidori journals every host call. §2 Replay returns recorded results without re-executing effects.";`,
  INSTRUCTIONS: `const INSTRUCTIONS = "You are a policy analyst. Answer only from the corpus and cite section numbers.";`,
  firstQuestion: `const firstQuestion = "What is a durable host call?";`,
  PERSONA: `const PERSONA = "You are the team's release-notes concierge: brisk, specific, honest.";`,
  draft: `const draft = "Draft announcement: chidori 0.4 ships crash recovery, cheaper replays, and actor supervision trees.";`,
  research: `const research = "Research notes: replay determinism holds across hosts; actor joins fold logs; prompt-cache hit rate 87%.";`,
  topic: `const topic = "prompt caching";`,
  payload: `const payload = { from: "a@example.com", subject: "hi" };`,
  learned: `const learned = { tone: "concise", citations: true };`,
  commits: `const commits = [{ subject: "fix replay divergence in retry helper" }, { subject: "actors: fold joined logs into the parent" }, { subject: "prompt cache: auto-mark the stable head" }];`,
  violations: `const violations = [];`,
  chidoriUrl: `const chidoriUrl = "http://127.0.0.1:8080";`,
  targetRun: `const targetRun = "sess-001";`,
  KEY: `const KEY = "sk-docs-vm-demo-key";`,
  pick: `const pick = (a, b) => (JSON.stringify(a).length >= JSON.stringify(b).length ? a : b);`,
  summarize: `const summarize = (violations) => (violations.length ? violations.join("; ") : "no violations found");`,
  buildIndex: `const buildIndex = (corpus) => ({ terms: String(corpus).toLowerCase().split(/\\W+/).filter(Boolean).slice(0, 50) });`,
  buildPlan: `const buildPlan = (input) => ({ steps: ["gather", "draft", "review"], input: input ?? null });`,
  buildPlanDeterministically: `const buildPlanDeterministically = (input) => ({ steps: ["gather", "draft", "review"], input: input ?? null });`,
  writeDraft: `const writeDraft = async (brief) => chidori.prompt("Write a short draft for this brief: " + JSON.stringify(brief), { type: "draft" });`,
  revise: `const revise = async (draft, notes, brief) => chidori.prompt("Revise the draft.\\n\\nDraft: " + draft + "\\nReviewer notes: " + notes + "\\nBrief: " + JSON.stringify(brief), { type: "draft" });`,
  runMaintenance: `const runMaintenance = async () => { await chidori.log("maintenance tick: state compacted"); };`,
  triage: `const triage = async (payload) => { await chidori.log("triaged", { payload, verdict: await chidori.prompt("Triage this in one sentence: " + JSON.stringify(payload)) }); };`,
  search: `const search = defineTool({ name: "docs_search", description: "Search the chidori docs.", parameters: { type: "object", properties: { query: { type: "string" } }, required: ["query"] }, run: async ({ query }) => chidori.tool("docs_search", { query }) });`,
  searchNotes: `const searchNotes = defineTool({ name: "search_notes", description: "Keyword search over the team's standup notes.", parameters: { type: "object", properties: { query: { type: "string" } }, required: ["query"] }, run: async ({ query }) => ["2026-05-04 standup: replay divergence bug traced to unseeded RNG.", "2026-05-11 standup: prompt cache hit rate at 87% after context() refactor.", "2026-05-18 standup: actors supervision tree shipped."].filter((n) => n.toLowerCase().includes(String(query).toLowerCase())) });`,
  wikiSearch: `const wikiSearch = defineTool({ name: "wiki_search", description: "Search Wikipedia and return the top matching titles and URLs.", parameters: { type: "object", properties: { query: { type: "string" } }, required: ["query"] }, run: async ({ query }) => { const resp = await fetch("https://en.wikipedia.org/w/api.php?action=opensearch&format=json&origin=*&limit=5&search=" + encodeURIComponent(query)); if (!resp.ok) return []; const [, titles, , urls] = await resp.json(); return titles.map((title, i) => ({ title, url: urls[i] })); } });`,
  client: `const client = { stream: async function* (input) { yield { type: "prompt_start", prompt_type: "progress", stream_id: "s1" }; let seq = 0; for (const delta of ["Chidori ", "streams ", "labelled ", "progress ", "events."]) yield { type: "prompt_delta", prompt_type: "progress", stream_id: "s1", seq: seq++, delta }; yield { type: "done", run_id: "run-docs-vm-1", status: "completed", input }; } };`,
  process: `const process = { stdout: { write: (s) => console.log(s) } };`,
};

/** FNV-1a, matching hashString in the playground's brain.ts and the docs
 *  runner's client-side lookup. */
function hashString(s) {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return (h >>> 0).toString(36);
}

const normalize = (code) => code.replace(/\r\n/g, '\n').trim();

/** Drop `import … from "chidori:agent"` and reference directives — the
 *  harness provides that surface. */
function stripAgentImport(code) {
  return code
    .replace(/^[ \t]*\/\/\/\s*<reference[^\n]*$/gm, '')
    .replace(/^[ \t]*import[^;]*?from\s*["']chidori:agent["'];?[ \t]*$/gm, '');
}

/** Drop imports of packages the harness stubs; report what they declare. */
function stripStubImports(code) {
  const declared = [];
  const stripped = code.replace(
    /^[ \t]*import\s+[^;]+?\s+from\s*["']([a-z0-9@/_-]+)["'];?[ \t]*$/gim,
    (line, pkg) => {
      if (STUB_PACKAGES[pkg]) {
        declared.push(STUB_PACKAGES[pkg]);
        return '';
      }
      return line;
    },
  );
  return { stripped, declared };
}

/**
 * Flat free-identifier analysis over the transpiled (type-erased) JS: every
 * value-position identifier that is neither declared anywhere in the block
 * nor an allowed global is "free". Flat scoping over-approximates what's
 * declared, which errs toward showing a Run button; a genuinely broken
 * block then fails visibly in the panel, which the short docs examples
 * never do in practice.
 */
function freeIdentifiers(sourceFile) {
  const declared = new Set();
  const referenced = new Set();

  const declareBinding = (name) => {
    if (!name) return;
    if (ts.isIdentifier(name)) declared.add(name.text);
    else if (ts.isObjectBindingPattern(name) || ts.isArrayBindingPattern(name)) {
      for (const el of name.elements) {
        if (ts.isBindingElement(el)) declareBinding(el.name);
      }
    }
  };

  const collectDeclarations = (node) => {
    if (ts.isVariableDeclaration(node) || ts.isBindingElement(node) || ts.isParameter(node)) {
      declareBinding(node.name);
    } else if (
      (ts.isFunctionDeclaration(node) || ts.isClassDeclaration(node) || ts.isFunctionExpression(node) || ts.isClassExpression(node)) &&
      node.name
    ) {
      declared.add(node.name.text);
    } else if (ts.isCatchClause(node) && node.variableDeclaration) {
      declareBinding(node.variableDeclaration.name);
    }
    ts.forEachChild(node, collectDeclarations);
  };

  const isReference = (node) => {
    const parent = node.parent;
    if (!parent) return true;
    // Property names, member names, and labels are not value references.
    if (ts.isPropertyAccessExpression(parent) && parent.name === node) return false;
    if (ts.isPropertyAssignment(parent) && parent.name === node) return false;
    if (
      (ts.isMethodDeclaration(parent) || ts.isPropertyDeclaration(parent) ||
        ts.isGetAccessor(parent) || ts.isSetAccessor(parent)) &&
      parent.name === node
    ) {
      return false;
    }
    if (ts.isBindingElement(parent) && parent.propertyName === node) return false;
    // Declaration sites were collected in the first pass.
    if (ts.isVariableDeclaration(parent) && parent.name === node) return false;
    if (ts.isBindingElement(parent) && parent.name === node) return false;
    if (ts.isParameter(parent) && parent.name === node) return false;
    if ((ts.isFunctionDeclaration(parent) || ts.isFunctionExpression(parent) || ts.isClassDeclaration(parent) || ts.isClassExpression(parent)) && parent.name === node) return false;
    if (ts.isLabeledStatement(parent) && parent.label === node) return false;
    if ((ts.isBreakStatement(parent) || ts.isContinueStatement(parent)) && parent.label === node) return false;
    return true;
  };

  const collectReferences = (node) => {
    if (ts.isIdentifier(node) && isReference(node)) referenced.add(node.text);
    ts.forEachChild(node, collectReferences);
  };

  collectDeclarations(sourceFile);
  collectReferences(sourceFile);

  const free = [];
  for (const name of referenced) {
    if (!declared.has(name) && !ALLOWED_GLOBALS.has(name)) free.push(name);
  }
  return free;
}

/** Sample values for common input-field names, so the flagship examples run
 *  meaningfully (and validation like `if (!topic) throw` passes) before the
 *  reader edits anything. */
const SAMPLE_INPUTS = {
  topic: 'durable agent runtimes',
  question: 'what happened with the prompt cache?',
  document: 'Chidori is a durable TypeScript agent runtime: every chidori.* call is journaled live and replayed from the log for $0.',
  corpus: '(sample corpus) §1 Chidori journals every host call. §2 Replay returns recorded results without re-executing effects.',
  name: 'reader',
  request: 'ship the TypeScript runtime',
  message: 'hello from the docs',
  task: "Reverse the word 'chidori' and tell me the result.",
};

/** Default value for a property, from its (erased-by-then) TS type text. */
function defaultForType(type) {
  if (!type) return '';
  if (ts.isUnionTypeNode(type)) return defaultForType(type.types[0]);
  switch (type.kind) {
    case ts.SyntaxKind.NumberKeyword:
      return 0;
    case ts.SyntaxKind.BooleanKeyword:
      return false;
    case ts.SyntaxKind.ArrayType:
      return [];
    case ts.SyntaxKind.TypeLiteral: {
      const nested = {};
      for (const m of type.members) {
        if (ts.isPropertySignature(m) && m.name && ts.isIdentifier(m.name)) {
          nested[m.name.text] = defaultForType(m.type);
        }
      }
      return nested;
    }
    default:
      return '';
  }
}

/**
 * Sniff the shape of the input object a program-mode example expects, from
 * the `run(handler)` signature: a typed parameter (`input: { question:
 * string }`) yields typed defaults, a type-alias reference resolves through
 * a same-block `type X = {…}`, a destructured one yields empty strings.
 */
function sniffInputShape(tsSourceFile) {
  const aliases = new Map();
  const collectAliases = (node) => {
    if (ts.isTypeAliasDeclaration(node) && ts.isTypeLiteralNode(node.type)) {
      aliases.set(node.name.text, node.type);
    }
    ts.forEachChild(node, collectAliases);
  };
  collectAliases(tsSourceFile);

  const shapeOfLiteral = (literal) => {
    const shape = {};
    for (const m of literal.members) {
      if (ts.isPropertySignature(m) && m.name && ts.isIdentifier(m.name)) {
        const fallback = defaultForType(m.type);
        shape[m.name.text] = fallback === '' && SAMPLE_INPUTS[m.name.text] ? SAMPLE_INPUTS[m.name.text] : fallback;
      }
    }
    return shape;
  };

  let shape = null;
  const visit = (node) => {
    if (
      shape === null &&
      ts.isCallExpression(node) &&
      ts.isIdentifier(node.expression) &&
      node.expression.text === 'run' &&
      node.arguments.length >= 1
    ) {
      const handler = node.arguments[0];
      if ((ts.isArrowFunction(handler) || ts.isFunctionExpression(handler)) && handler.parameters.length >= 1) {
        const param = handler.parameters[0];
        if (param.type && ts.isTypeLiteralNode(param.type)) {
          shape = shapeOfLiteral(param.type);
        } else if (
          param.type &&
          ts.isTypeReferenceNode(param.type) &&
          ts.isIdentifier(param.type.typeName) &&
          aliases.has(param.type.typeName.text)
        ) {
          shape = shapeOfLiteral(aliases.get(param.type.typeName.text));
        } else if (ts.isObjectBindingPattern(param.name)) {
          shape = {};
          for (const el of param.name.elements) {
            if (ts.isBindingElement(el) && ts.isIdentifier(el.name)) shape[el.name.text] = SAMPLE_INPUTS[el.name.text] ?? '';
          }
        } else {
          shape = {};
        }
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(tsSourceFile);
  return shape && Object.keys(shape).length ? shape : null;
}

function* mdFiles(dir, rel = '') {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const relPath = rel ? `${rel}/${entry.name}` : entry.name;
    if (entry.isDirectory()) {
      if (entry.name === 'node_modules' || entry.name === 'media') continue;
      yield* mdFiles(path.join(dir, entry.name), relPath);
    } else if (entry.name.endsWith('.md') && !EXCLUDE.has(relPath)) {
      yield relPath;
    }
  }
}

function analyzeCodeBlock(code) {
  const noAgent = stripAgentImport(code);
  const { stripped, declared: stubDeclared } = stripStubImports(noAgent);
  // Any surviving module syntax means the block needs things the sandbox
  // doesn't have (other modules, an importer).
  if (/^[ \t]*(import|export)\b/m.test(stripped)) return null;

  const apiCalls = [...stripped.matchAll(/\bchidori\s*\.\s*(\w+)/g)].map((m) => m[1]);
  if (apiCalls.some((name) => !SUPPORTED_API.has(name))) return null;

  // Reference listings show call alternates as repeated `const x = …` lines;
  // that's a runtime redeclaration error, so those blocks stay static.
  const declCounts = new Map();
  for (const m of stripped.matchAll(/^[ \t]*(?:const|let)\s+(\w+)\s*=/gm)) {
    declCounts.set(m[1], (declCounts.get(m[1]) ?? 0) + 1);
  }
  if ([...declCounts.values()].some((n) => n > 1)) return null;

  // First pass: what's free without ambient help?
  const bareWrapped = `async function __example() {\n${stripped}\n}\n`;
  const bareTranspiled = ts.transpileModule(bareWrapped, {
    reportDiagnostics: true,
    compilerOptions: { target: ts.ScriptTarget.ES2020, module: ts.ModuleKind.ESNext },
  });
  if (bareTranspiled.diagnostics && bareTranspiled.diagnostics.length > 0) return null;

  const bareAst = ts.createSourceFile('block.js', bareTranspiled.outputText, ts.ScriptTarget.ES2022, true, ts.ScriptKind.JS);
  const free = freeIdentifiers(bareAst).filter((name) => !stubDeclared.includes(name));

  // Resolve remaining free identifiers from the ambient table (order fixed
  // by the table so interdependent stubs stay stable).
  const unresolved = free.filter((name) => !AMBIENT[name]);
  if (unresolved.length > 0) return null;
  const ambient = Object.keys(AMBIENT)
    .filter((name) => free.includes(name))
    .map((name) => AMBIENT[name])
    .join('\n');

  // Must actually exercise the runtime (ambient stubs count: a fragment
  // driving a spawned `worker` exercises it through the stub) — bare
  // type/interface blocks erase to nothing and would "run" vacuously.
  if (!/\bchidori\s*\.|\bfetch\s*\(|\brun\s*\(|\bclient\s*\.\s*stream\s*\(/.test(ambient + bareTranspiled.outputText)) return null;

  // Check the exact shape the harness executes: ambient + block inlined into
  // an async function body (which also legalizes top-level await/return).
  if (ambient) {
    const wrapped = `async function __example() {\n${ambient}\n${stripped}\n}\n`;
    const transpiled = ts.transpileModule(wrapped, {
      reportDiagnostics: true,
      compilerOptions: { target: ts.ScriptTarget.ES2020, module: ts.ModuleKind.ESNext },
    });
    if (transpiled.diagnostics && transpiled.diagnostics.length > 0) return null;
    const ast = ts.createSourceFile('block.js', transpiled.outputText, ts.ScriptTarget.ES2022, true, ts.ScriptKind.JS);
    if (freeIdentifiers(ast).length > 0) return null;
  }

  const mode = /\brun\s*\(/.test(stripped) ? 'program' : 'fragment';
  let input = null;
  if (mode === 'program') {
    const tsAst = ts.createSourceFile('block.ts', `async function __x() {\n${stripped}\n}\n`, ts.ScriptTarget.ES2022, true, ts.ScriptKind.TS);
    input = sniffInputShape(tsAst);
  }
  return { mode, ...(input ? { input } : {}), ...(ambient ? { ambient } : {}) };
}

// ---------------------------------------------------------------------------
// VM filesystem seeds.

/** Small agents for the sources docs examples spawn/branch/call by path. */
const HANDWRITTEN_SEEDS = {
  'workers/researcher.ts': `import { chidori, run } from "chidori:agent";

// A worker actor: picks up any queued steer, researches its topic, reports
// progress + findings + a draft to its spawner, then settles.
run(async (input: { topic?: string; angle?: string; id?: number }) => {
  const focus = await chidori.pollSignal("focus");
  const findings = await chidori.prompt(
    "Write three crisp research findings on " + String(input.topic ?? "the topic") +
      (input.angle ? " from this angle: " + input.angle : "") +
      (focus ? " (steered: " + JSON.stringify(focus.payload) + ")" : ""),
    { type: "progress" },
  );
  await chidori.actors.send("parent", "progress", { id: input.id ?? 1, note: "drafting" });
  await chidori.actors.send("parent", "finding", { angle: input.angle ?? "general", findings, toolCalls: 0 });
  await chidori.actors.send("parent", "draft", { topic: input.topic ?? null, findings });
  return { topic: input.topic ?? null, findings };
});
`,
  'workers/critic.ts': `import { chidori, run } from "chidori:agent";

run(async (input: { topic?: string; dossier?: string }) => {
  const review = await chidori.pollSignal("review");
  const material = input.dossier ?? (review ? JSON.stringify(review.payload) : "(no dossier handed in)");
  const critique = await chidori.prompt(
    "Critique this dossier in three specific points:\\n\\n" + material,
    { type: "progress" },
  );
  await chidori.actors.send("parent", "critique", { critique });
  return { critique };
});
`,
  'services/inbox-triager.ts': `// A detached service: hibernates between emails, holding no thread and no
// VM; wake it with a "email" signal, stop it with "shutdown".
import { chidori, run } from "chidori:agent";

run(async () => {
  const triaged = [];
  for (;;) {
    const msg = await chidori.signal(["email", "shutdown"], { timeoutMs: 6 * 60 * 60 * 1000 });
    if (msg.timedOut) { await chidori.log("maintenance tick", { triaged: triaged.length }); continue; }
    if (msg.name === "shutdown") return { triaged: triaged.length };
    const result = await chidori.prompt("Triage: " + JSON.stringify(msg.payload));
    triaged.push({ email: msg.payload, result });
    await chidori.log("triaged", { count: triaged.length });
  }
});
`,
  'child.ts': `import { chidori, run } from "chidori:agent";

run(async (input: { topic?: string }) => {
  const answer = await chidori.prompt("Answer briefly about: " + JSON.stringify(input));
  return { answer };
});
`,
  'worker.ts': `import { chidori, run } from "chidori:agent";

run(async (input: { shard?: string }) => {
  const result = await chidori.prompt(
    "Process shard " + String(input.shard ?? "?") + " and summarize the outcome in one line.",
    { type: "progress" },
  );
  return { shard: input.shard ?? null, result };
});
`,
  'prompts/summary.jinja': `Summarize the following document in three bullets:

{{ document }}
`,
  'notes/draft.md': `# Draft notes

Chidori journals every host call; replay returns recorded results without
re-executing effects. This file exists so workspace examples have something
to read before they write.
`,
  'README.md': `# chidori docs VM

A simulated Linux filesystem in your browser. The \`chidori\` CLI is real —
agents run on the wasm build of the chidori engine and journal to
.chidori/runs/. Start with:

    chidori run examples/agents/hello.ts --input name=you
`,
};

for (const strategy of ['outline_first', 'draft_direct', 'exec_brief', 'feature_story']) {
  const label = strategy.replace('_', '-');
  HANDWRITTEN_SEEDS[`strategies/${strategy}.ts`] = `import { chidori, run } from "chidori:agent";

run(async (input: { topic?: string; research?: string; dossier?: string; critique?: string }) => {
  const draft = await chidori.prompt(
    "Write a draft using the ${label} strategy. Material: " + JSON.stringify(input),
    { type: "draft" },
  );
  return { strategy: "${label}", draft };
});
`;
}

function collectRepoExampleSeeds() {
  const seeds = {};
  const addDir = (absDir, relPrefix, filter) => {
    if (!fs.existsSync(absDir)) return;
    for (const entry of fs.readdirSync(absDir, { withFileTypes: true })) {
      const abs = path.join(absDir, entry.name);
      if (entry.isDirectory()) addDir(abs, `${relPrefix}${entry.name}/`, filter);
      else if (filter.test(entry.name)) seeds[`${relPrefix}${entry.name}`] = fs.readFileSync(abs, 'utf8');
    }
  };
  addDir(path.join(examplesDir, 'agents'), 'examples/agents/', /\.ts$/);
  addDir(path.join(examplesDir, 'prompts'), 'examples/prompts/', /\.(jinja|j2)$/);
  return seeds;
}

/** The agent a page's `chidori run <file>` means: the page's own ts block. */
function pageDerivedSeeds(pages) {
  const seeds = {};
  for (const { blocks, shellText } of pages) {
    const referenced = new Set();
    for (const m of shellText.matchAll(/chidori\s+(?:run|serve|check|chat|resume|verify)\s+(?:--\S+\s+)*([\w./-]+\.ts)/g)) {
      referenced.add(m[1].replace(/^\.\//, ''));
    }
    const programs = blocks.filter((b) => /\brun\s*\(/.test(b) && /chidori:agent/.test(b));
    let next = 0;
    for (const rel of [...referenced].sort()) {
      if (rel.startsWith('examples/') || seeds[rel] || HANDWRITTEN_SEEDS[rel]) continue;
      const code = programs[next] ?? programs[programs.length - 1];
      seeds[rel] =
        code ??
        `import { chidori, run } from "chidori:agent";\n\nrun(async (input: { name?: string }) => {\n  await chidori.log("running ${rel}", { input });\n  return { ok: true, agent: "${rel}" };\n});\n`;
      if (code) next = Math.min(next + 1, programs.length - 1);
    }
  }
  return seeds;
}

// ---------------------------------------------------------------------------

const examples = {};
let scannedCode = 0;
let runnableCode = 0;
let shellBlocks = 0;
const pages = [];

for (const rel of [...mdFiles(docsDir)].sort()) {
  const raw = fs.readFileSync(path.join(docsDir, rel), 'utf8');
  const blocks = [];
  let shellText = '';
  for (const m of raw.matchAll(/^([ \t]*)```([a-zA-Z]+)[^\n]*\n([\s\S]*?)^[ \t]*```/gm)) {
    const lang = m[2].toLowerCase();
    // Blocks nested in list items are fence-indented in the markdown but
    // render dedented — strip the fence's indent so hashes match the DOM.
    const indent = m[1];
    const code = indent
      ? m[3].split('\n').map((l) => (l.startsWith(indent) ? l.slice(indent.length) : l)).join('\n')
      : m[3];
    if (lang === 'ts' || lang === 'js' || lang === 'typescript' || lang === 'javascript') {
      scannedCode += 1;
      blocks.push(code);
      const entry = analyzeCodeBlock(code);
      if (!entry) continue;
      runnableCode += 1;
      examples[hashString(normalize(code))] = entry;
    } else if (lang === 'bash' || lang === 'sh' || lang === 'shell' || lang === 'console') {
      shellBlocks += 1;
      shellText += `\n${code}`;
      examples[hashString(normalize(code))] = { mode: 'shell' };
    }
  }
  pages.push({ page: rel, blocks, shellText });
}

const seeds = {
  ...collectRepoExampleSeeds(),
  ...HANDWRITTEN_SEEDS,
  ...pageDerivedSeeds(pages),
};

fs.mkdirSync(path.dirname(outIndex), { recursive: true });
fs.writeFileSync(outIndex, JSON.stringify({ examples }));
fs.writeFileSync(outSeed, JSON.stringify({ files: seeds }));
console.log(
  `runnable docs examples: ${runnableCode} of ${scannedCode} ts/js blocks + ${shellBlocks} shell blocks → ${path.relative(process.cwd(), outIndex)}`,
);
console.log(`docs VM seed: ${Object.keys(seeds).length} files → ${path.relative(process.cwd(), outSeed)}`);
