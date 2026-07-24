// Builds public/runnable-examples.json — an index of the docs' ts/js code
// blocks that can actually execute on the wasm engine in the reader's
// browser. Docs pages look their code blocks up in it (by content hash) and
// offer a "Run" button that opens the example-runner side panel.
//
// A block qualifies when, after dropping its `chidori:agent` import, it is
// syntactically valid, references nothing the runner's sandbox doesn't
// provide (free-identifier analysis via the TypeScript parser), and touches
// only the durable-core chidori API the browser host implements
// (prompt/input/tool/log/step/sleep/now/random + captured fetch). Fragments
// without a `run(...)` registration are executed as-is inside an async
// wrapper; programs get their registered handler invoked with a reader-
// editable input object, whose shape is sniffed from the handler signature.
//
// Runs before `next dev`/`next build` (see package.json); the output is
// gitignored like the wasm assets.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import ts from 'typescript';

const here = path.dirname(fileURLToPath(import.meta.url));
const docsDir = path.resolve(here, '../../docs');
const outFile = path.resolve(here, '../public/runnable-examples.json');

// Mirror the site's content rules (source.config.ts): every docs/*.md except
// the GitHub-facing README and the posts thread, which are not site pages.
const EXCLUDE = new Set(['README.md', 'posts/harness-engineering-thread.md']);

// The slice of `chidori.*` the browser host implements (sdk/browser). A block
// that touches anything else (context, actors, memory, workspace, signals…)
// stays a static listing.
const SUPPORTED_API = new Set([
  'prompt',
  'input',
  'tool',
  'log',
  'step',
  'sleep',
  'now',
  'random',
  'fetch',
]);

// Globals the runner's VM provides: the harness surface (chidori/run/
// defineTool/fetch), plus engine built-ins. Deliberately tight — a block
// referencing anything outside this list is skipped rather than shown with
// a Run button that throws.
const ALLOWED_GLOBALS = new Set([
  'chidori', 'run', 'defineTool', 'fetch', 'console',
  'JSON', 'Math', 'Object', 'Array', 'String', 'Number', 'Boolean', 'Symbol',
  'Promise', 'Date', 'RegExp', 'Map', 'Set', 'WeakMap', 'WeakSet',
  'Error', 'TypeError', 'RangeError', 'SyntaxError',
  'parseInt', 'parseFloat', 'isNaN', 'isFinite',
  'encodeURIComponent', 'decodeURIComponent', 'encodeURI', 'decodeURI',
  'NaN', 'Infinity', 'undefined', 'globalThis', 'arguments',
]);

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

/** Drop `import … from "chidori:agent"` — the harness provides that surface. */
function stripAgentImport(code) {
  return code.replace(/^[ \t]*import[^;]*?from\s*["']chidori:agent["'];?[ \t]*$/gm, '');
}

/**
 * Flat free-identifier analysis over the transpiled (type-erased) JS: every
 * value-position identifier that is neither declared anywhere in the block
 * nor an allowed global makes the block non-runnable. Flat scoping
 * over-approximates what's declared, which errs toward showing a Run button;
 * a genuinely broken block then fails visibly in the panel, which the short
 * docs examples never do in practice.
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
 * string }`) yields typed defaults, a destructured one yields empty strings.
 * Returns null when the handler takes no input.
 */
function sniffInputShape(tsSourceFile) {
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
          shape = {};
          for (const m of param.type.members) {
            if (ts.isPropertySignature(m) && m.name && ts.isIdentifier(m.name)) {
              shape[m.name.text] = defaultForType(m.type);
            }
          }
        } else if (ts.isObjectBindingPattern(param.name)) {
          shape = {};
          for (const el of param.name.elements) {
            if (ts.isBindingElement(el) && ts.isIdentifier(el.name)) shape[el.name.text] = '';
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

function analyzeBlock(code) {
  const stripped = stripAgentImport(code);
  // Any surviving module syntax means the block needs things the sandbox
  // doesn't have (other modules, an importer).
  if (/^[ \t]*(import|export)\b/m.test(stripped)) return null;

  const apiCalls = [...stripped.matchAll(/\bchidori\s*\.\s*(\w+)/g)].map((m) => m[1]);
  if (apiCalls.some((name) => !SUPPORTED_API.has(name))) return null;

  // Check the exact shape the harness executes: the block inlined into an
  // async function body (which also legalizes top-level await/return).
  const wrapped = `async function __example() {\n${stripped}\n}\n`;
  const transpiled = ts.transpileModule(wrapped, {
    reportDiagnostics: true,
    compilerOptions: { target: ts.ScriptTarget.ES2020, module: ts.ModuleKind.ESNext },
  });
  if (transpiled.diagnostics && transpiled.diagnostics.length > 0) return null;

  // Must actually exercise the runtime — bare type/interface blocks erase to
  // nothing and would "run" vacuously.
  if (!/\bchidori\s*\.|\bfetch\s*\(|\brun\s*\(/.test(transpiled.outputText)) return null;

  const jsAst = ts.createSourceFile('block.js', transpiled.outputText, ts.ScriptTarget.ES2022, true, ts.ScriptKind.JS);
  if (freeIdentifiers(jsAst).length > 0) return null;

  const mode = /\brun\s*\(/.test(stripped) ? 'program' : 'fragment';
  let input = null;
  if (mode === 'program') {
    const tsAst = ts.createSourceFile('block.ts', wrapped, ts.ScriptTarget.ES2022, true, ts.ScriptKind.TS);
    input = sniffInputShape(tsAst);
  }
  return { mode, ...(input ? { input } : {}) };
}

const examples = {};
let scanned = 0;
let runnable = 0;
for (const rel of [...mdFiles(docsDir)].sort()) {
  const raw = fs.readFileSync(path.join(docsDir, rel), 'utf8');
  for (const m of raw.matchAll(/```(ts|js|typescript|javascript)[^\n]*\n([\s\S]*?)```/g)) {
    scanned += 1;
    const code = m[2];
    const entry = analyzeBlock(code);
    if (!entry) continue;
    runnable += 1;
    examples[hashString(normalize(code))] = entry;
  }
}

fs.mkdirSync(path.dirname(outFile), { recursive: true });
fs.writeFileSync(outFile, JSON.stringify({ examples }));
console.log(
  `runnable docs examples: ${runnable} of ${scanned} ts/js blocks → ${path.relative(process.cwd(), outFile)}`,
);
