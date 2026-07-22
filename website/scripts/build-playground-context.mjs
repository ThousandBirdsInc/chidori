// Builds public/playground-docs.json — a plain-text index of docs/ that the
// /playground chat agent loads at runtime: it grounds the LLM's system prompt
// and serves the agent's `search_docs` tool. Runs before `next dev`/`next
// build` (see package.json); the output is gitignored like the wasm assets.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const docsDir = path.resolve(here, '../../docs');
const outFile = path.resolve(here, '../public/playground-docs.json');

// Mirror the site's content rules (source.config.ts): every docs/*.md except
// the GitHub-facing README and the posts thread, which are not site pages.
const EXCLUDE = new Set(['README.md', 'posts/harness-engineering-thread.md']);
// Internal review/meta artifacts stay on the site but out of the agent's
// context — their headings ("Blocker 3: ...") drown out the real docs when
// retrieving grounding for a question.
const EXCLUDE_PATTERNS = [/^consumer-usability-review/, /^ai-sdk-gap-analysis/];
const MAX_SECTION_CHARS = 1600;
const MIN_SECTION_CHARS = 40;

function* mdFiles(dir, rel = '') {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const relPath = rel ? `${rel}/${entry.name}` : entry.name;
    if (entry.isDirectory()) {
      if (entry.name === 'node_modules' || entry.name === 'media') continue;
      yield* mdFiles(path.join(dir, entry.name), relPath);
    } else if (
      entry.name.endsWith('.md') &&
      !EXCLUDE.has(relPath) &&
      !EXCLUDE_PATTERNS.some((p) => p.test(relPath))
    ) {
      yield relPath;
    }
  }
}

/** Markdown → searchable plain text (code block contents are kept: agent
 *  API questions often match on identifiers). */
function toText(md) {
  return md
    .replace(/```[^\n]*\n([\s\S]*?)```/g, '$1')
    .replace(/`([^`]*)`/g, '$1')
    .replace(/!\[[^\]]*\]\([^)]*\)/g, '')
    .replace(/\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/^[ \t]*[|>#*-]+[ \t]*/gm, '')
    .replace(/[*_]{1,3}([^*_]+)[*_]{1,3}/g, '$1')
    .replace(/[ \t]+/g, ' ')
    .replace(/\n{2,}/g, '\n')
    .trim();
}

const pages = [];
for (const rel of [...mdFiles(docsDir)].sort()) {
  let raw = fs.readFileSync(path.join(docsDir, rel), 'utf8');

  let title = '';
  const fm = /^---\n([\s\S]*?)\n---\n/.exec(raw);
  if (fm) {
    const m = /^title:\s*["']?(.+?)["']?\s*$/m.exec(fm[1]);
    if (m) title = m[1];
    raw = raw.slice(fm[0].length);
  }
  const h1 = /^#\s+(.+)$/m.exec(raw);
  if (!title) title = h1 ? h1[1].trim() : rel.replace(/\.md$/, '');

  const slug = rel.replace(/\.md$/, '');
  const route = slug === 'index' ? '/docs' : `/docs/${slug}`;

  // One section per `##` heading, plus the pre-heading intro.
  const sections = [];
  let heading = '';
  let buf = [];
  const flush = () => {
    const text = toText(buf.join('\n')).slice(0, MAX_SECTION_CHARS);
    if (text.length >= MIN_SECTION_CHARS) sections.push({ heading, text });
    buf = [];
  };
  for (const line of raw.split('\n')) {
    const h = /^##\s+(.+)$/.exec(line);
    if (h) {
      flush();
      heading = h[1].trim();
    } else {
      buf.push(line);
    }
  }
  flush();
  if (sections.length) pages.push({ slug, route, title, sections });
}

fs.mkdirSync(path.dirname(outFile), { recursive: true });
fs.writeFileSync(outFile, JSON.stringify({ pages }));
const kb = Math.round(fs.statSync(outFile).size / 1024);
console.log(`playground docs context: ${pages.length} pages → ${path.relative(process.cwd(), outFile)} (${kb} KB)`);
