import { defineConfig, defineDocs } from 'fumadocs-mdx/config';
import path from 'node:path';
import { visit } from 'unist-util-visit';
import type { Root } from 'mdast';
import type { VFile } from 'vfile';
import type { Transformer } from 'unified';

const REPO = 'https://github.com/ThousandBirdsInc/chidori';
const DOCS_DIR = path.resolve(process.cwd(), '../docs');

/**
 * Repo-only pages: kept in docs/ for contributors (readable on GitHub) but
 * not built into the site. Internal engineering notes and the historical
 * usability-review logs live here; links to them from site pages are
 * rewritten to GitHub URLs below.
 */
const REPO_ONLY_SLUGS = new Set([
  'README',
  'posts/harness-engineering-thread',
  'interpreter-optimization',
  'js-performance-roadmap',
  'js-object-shapes-design',
  'jit',
  'os-isolation-plan',
  'resume-performance',
  'dom-runtime-prototype',
  'ai-sdk-gap-analysis',
  'rust-style-guide',
  'releasing',
  'consumer-usability-review',
  'consumer-usability-review-2',
  'consumer-usability-review-3',
  'consumer-usability-review-4',
  'consumer-usability-review-5',
  'consumer-usability-review-6',
]);

/**
 * The content in ../docs is plain CommonMark written to be readable on
 * GitHub, so pages link to each other as `./page.md` and to files elsewhere
 * in the repo as `../examples/...`. Rewrite both at compile time:
 *   - links to other docs pages become site routes (/docs/<slug>)
 *   - links that leave docs/ become GitHub URLs
 */
function remarkRepoLinks(): Transformer<Root> {
  return (tree, file) => {
    const vfile = file as VFile;
    const fromDir = path
      .relative(DOCS_DIR, path.dirname(vfile.path))
      .split(path.sep)
      .join('/');
    visit(tree, ['link', 'image', 'definition'], (node) => {
      if (!('url' in node) || typeof node.url !== 'string') return;
      const url = node.url;
      if (!url || /^[a-z][a-z0-9+.-]*:/i.test(url) || url.startsWith('#') || url.startsWith('/')) {
        return;
      }
      const [target, hash] = url.split('#');
      const anchor = hash ? `#${hash}` : '';
      const resolved = path.posix.normalize(path.posix.join('docs', fromDir, target));
      if (resolved.startsWith('docs/') && resolved.endsWith('.md')) {
        const slug = resolved.slice('docs/'.length, -'.md'.length);
        if (!REPO_ONLY_SLUGS.has(slug)) {
          node.url = slug === 'index' ? `/docs${anchor}` : `/docs/${slug}${anchor}`;
          return;
        }
      }
      if (node.type === 'image') {
        node.url = `https://raw.githubusercontent.com/ThousandBirdsInc/chidori/main/${resolved}`;
        return;
      }
      const kind = target.endsWith('/') ? 'tree' : 'blob';
      node.url = `${REPO}/${kind}/main/${resolved}${anchor}`;
    });
  };
}

export const docs = defineDocs({
  dir: '../docs',
  docs: {
    files: [
      '**/*.md',
      ...[...REPO_ONLY_SLUGS].map((slug) => `!${slug}.md`),
      '!node_modules/**',
    ],
  },
  meta: {
    // docs/media holds animation data JSON; only meta.json files describe
    // the page tree.
    files: ['**/meta.json'],
  },
});

export default defineConfig({
  mdxOptions: {
    // Note: `format` must stay unset — .md files are compiled in plain
    // markdown mode by extension, so `{` and `<` in prose stay literal.
    // remark-image wants to resolve image files locally; ours rewrite to
    // GitHub raw URLs instead.
    remarkImageOptions: false,
    remarkPlugins: (v) => [remarkRepoLinks, ...v],
  },
});
