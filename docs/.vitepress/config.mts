import { defineConfig } from 'vitepress'
import type MarkdownIt from 'markdown-it'

const REPO = 'https://github.com/ThousandBirdsInc/chidori'

// The markdown files in docs/ are written to be readable on GitHub, so they
// link to files elsewhere in the repo (../examples/..., ../sdk/...). Those
// targets are outside the site root; rewrite them to GitHub URLs at render
// time instead of shipping broken links.
function rewriteOutOfTreeLinks(md: MarkdownIt) {
  const fallback: NonNullable<MarkdownIt['renderer']['rules']['link_open']> = (
    tokens,
    idx,
    options,
    _env,
    self,
  ) => self.renderToken(tokens, idx, options)
  const orig = md.renderer.rules.link_open || fallback
  md.renderer.rules.link_open = (tokens, idx, options, env, self) => {
    const token = tokens[idx]
    const href = token.attrGet('href')
    if (href && /^\.\.?\//.test(href)) {
      const page: string = env.relativePath || ''
      const resolved = new URL(href, `file:///docs/${page}`)
      if (!resolved.pathname.startsWith('/docs/')) {
        const kind = resolved.pathname.endsWith('/') ? 'tree' : 'blob'
        token.attrSet('href', `${REPO}/${kind}/main${resolved.pathname}${resolved.hash}`)
      }
    }
    return orig(tokens, idx, options, env, self)
  }
}

// Several docs show Jinja syntax (`{{ var }}`) in inline code. VitePress only
// applies v-pre to fenced code blocks, so Vue would interpolate mustaches in
// inline code at SSR time and crash the page render. Mark every inline code
// span v-pre — docs prose never needs Vue interpolation.
function vPreInlineCode(md: MarkdownIt) {
  const orig = md.renderer.rules.code_inline!
  md.renderer.rules.code_inline = (tokens, idx, options, env, self) => {
    tokens[idx].attrSet('v-pre', '')
    return orig(tokens, idx, options, env, self)
  }
}

export default defineConfig({
  title: 'Chidori',
  description:
    'The agent framework where every run is durable, replayable, and resumable by default.',
  // For GitHub Pages project hosting the CI workflow sets DOCS_BASE=/chidori/.
  base: process.env.DOCS_BASE || '/',
  srcExclude: ['posts/harness-engineering-thread.md'],
  // The theme matches thousandbirds.ai, which is dark-only.
  appearance: 'force-dark',
  lastUpdated: true,
  markdown: {
    config(md) {
      rewriteOutOfTreeLinks(md)
      vPreInlineCode(md)
    },
  },
  themeConfig: {
    nav: [
      { text: 'Guide', link: '/getting-started' },
      { text: 'Examples', link: `${REPO}/tree/main/examples` },
    ],
    sidebar: [
      {
        text: 'Using Chidori',
        items: [
          { text: 'Getting Started', link: '/getting-started' },
          { text: 'Core Concepts', link: '/core-concepts' },
          { text: 'Replay & Resume', link: '/replay' },
          { text: 'Running Modes', link: '/running-modes' },
          { text: 'Signals', link: '/signals' },
          { text: 'Branching Execution', link: '/branching-execution' },
          { text: 'Actors', link: '/actors' },
          { text: 'Detached Agents', link: '/detached-agents' },
          { text: 'Context Management', link: '/context-management' },
          { text: 'Memory', link: '/memory' },
          { text: 'Prompt Templates', link: '/template' },
          { text: 'Value Checkpoints', link: '/value-checkpoints' },
          { text: 'Durable Storage', link: '/durable-storage' },
          { text: 'Package Management', link: '/package-management' },
          { text: 'Sandbox Model', link: '/sandbox-model' },
          { text: 'Observing with Tael', link: '/observing-with-tael' },
          { text: 'Deployment', link: '/deployment' },
        ],
      },
      {
        text: 'Engineering Notes',
        collapsed: true,
        items: [
          { text: 'Architecture', link: '/architecture' },
          { text: 'Conformance (Test262)', link: '/conformance' },
          { text: 'Captured Effects', link: '/captured-effects-vfs-crypto-timers' },
          { text: 'Interpreter Optimization', link: '/interpreter-optimization' },
          { text: 'JS Performance Roadmap', link: '/js-performance-roadmap' },
          { text: 'Object Shapes Design', link: '/js-object-shapes-design' },
          { text: 'JIT Experiment (retired)', link: '/jit' },
          { text: 'OS Isolation Plan', link: '/os-isolation-plan' },
          { text: 'Resume Performance', link: '/resume-performance' },
          { text: 'DOM Runtime Prototype', link: '/dom-runtime-prototype' },
          { text: 'AI SDK Gap Analysis', link: '/ai-sdk-gap-analysis' },
          { text: 'Rust Style Guide', link: '/rust-style-guide' },
          { text: 'Releasing', link: '/releasing' },
        ],
      },
      {
        text: 'Usability Reviews',
        collapsed: true,
        items: [
          { text: 'Round 1: Linear Path', link: '/consumer-usability-review' },
          { text: 'Round 2: Multi-Agent Surface', link: '/consumer-usability-review-2' },
          { text: 'Round 3: Daily Driver', link: '/consumer-usability-review-3' },
          { text: 'Round 4: Day-2 Surface', link: '/consumer-usability-review-4' },
          { text: 'Round 5: Shipping to Users', link: '/consumer-usability-review-5' },
          { text: 'Round 6: Conversational Surface', link: '/consumer-usability-review-6' },
        ],
      },
      {
        text: 'Posts',
        collapsed: true,
        items: [
          {
            text: 'Harness Engineering Needs a Substrate',
            link: '/posts/harness-engineering-needs-a-substrate',
          },
        ],
      },
    ],
    socialLinks: [
      { icon: 'github', link: REPO },
      { icon: 'discord', link: 'https://discord.gg/CJwKsPSgew' },
    ],
    search: {
      provider: 'local',
    },
    editLink: {
      pattern: `${REPO}/edit/main/docs/:path`,
      text: 'Edit this page on GitHub',
    },
    outline: {
      level: [2, 3],
    },
    footer: {
      message: 'Released under the Apache-2.0 License.',
      copyright: 'Copyright © Thousand Birds, Inc.',
    },
  },
})
