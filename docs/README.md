# Chidori documentation

Everything here is plain markdown, readable on GitHub as-is. The same files
are the content source for the docs website in [`website/`](../website),
built with [Next.js](https://nextjs.org) + [Fumadocs](https://fumadocs.dev):

```bash
cd website
npm install
npm run dev     # local dev server with live reload
npm run build   # static site in website/out
```

The site is deployed to GitHub Pages by
[`.github/workflows/docs.yml`](../.github/workflows/docs.yml) on every push to
`main` that touches `docs/` or `website/`.

Conventions for writing pages:

- Every page carries a small YAML frontmatter block with its sidebar `title`;
  keep the `# H1` in the body too — that's what renders, on GitHub and on the
  site.
- Sidebar order and section groupings live in `meta.json` (and
  `posts/meta.json`).
- Keep writing ordinary relative links (`./other-page.md`,
  `../examples/...`); the build rewrites in-docs links to site routes and
  out-of-docs links to GitHub URLs.
- Write plain CommonMark, not MDX — `{` and `<` in prose stay literal.
- Link to sibling pages with real markdown links, never inert code spans;
  point "API reference" mentions at [host-api.md](./host-api.md) (`llm.txt`
  is the LLM-consumable copy, not the human reference).
- Pages under **Using Chidori** are written for agent authors and operators:
  no Rust source paths, internal type names, test inventories, or roadmap
  sections. Engineering material belongs in the repo-only notes below.
- The site build ships only the sections listed in `meta.json`. This README
  and the repo-only notes below are excluded in
  [`website/source.config.ts`](../website/source.config.ts); links to them
  from site pages are rewritten to GitHub URLs automatically.

The sidebar starts at [index.md](./index.md), the site's overview page.

## Using Chidori

Guides for agent authors and operators, roughly in reading order:

| Doc | What it covers |
|---|---|
| [getting-started.md](./getting-started.md) | Install, sign in to a provider, scaffold and run your first agent |
| [your-first-agent.md](./your-first-agent.md) | Tutorial: write an agent, pause it, replay it for $0, check it into CI |
| [core-concepts.md](./core-concepts.md) | Agents, host functions, the journal, and the mental model |
| [patterns.md](./patterns.md) | Task-oriented recipes: which primitive fits which job |
| [faq.md](./faq.md) | Python support, Node, providers, comparisons, data locality, troubleshooting |
| [replay.md](./replay.md) | Record, replay, resume, divergence rules, replay tests |
| [running-modes.md](./running-modes.md) | `run` vs `serve`, sessions, the HTTP endpoint reference |
| [signals.md](./signals.md) | Named signals: pause for humans or other agents |
| [branching-execution.md](./branching-execution.md) | `chidori.branch` sub-runs |
| [source-history.md](./source-history.md) | The recorded chain of source versions behind every run |
| [actors.md](./actors.md) | Supervised, message-passing agent processes |
| [detached-agents.md](./detached-agents.md) | Long-lived agents outside a session |
| [context-management.md](./context-management.md) | Conversation contexts, prompt caching, compaction |
| [memory.md](./memory.md) | `chidori.memory`: persistent cross-run key-value storage |
| [template.md](./template.md) | `chidori.template`: Jinja prompt rendering |
| [value-checkpoints.md](./value-checkpoints.md) | `chidori.step`: bounding replay cost |
| [durable-storage.md](./durable-storage.md) | Run persistence, run-store backends, time travel |
| [package-management.md](./package-management.md) | Imports, `node:` builtins, npm packages |
| [sandbox-model.md](./sandbox-model.md) | The security model and its guarantees |
| [observing-with-tael.md](./observing-with-tael.md) | OTLP export, run↔trace correlation, golden cases |
| [deployment.md](./deployment.md) | Serving agents in production |
| [browser-agents.md](./browser-agents.md) | The wasm build: agents in the browser |

## Reference

| Doc | What it covers |
|---|---|
| [host-api.md](./host-api.md) | Every `chidori.*` method, option by option; providers; runtime policy |
| [cli.md](./cli.md) | Every subcommand, flag by flag |

## Internals

Contributor-facing pages that user docs link into; these stay on the site:

- [architecture.md](./architecture.md) — engine + runtime layering
- [conformance.md](./conformance.md) — Test262 methodology and CI gate
- [captured-effects-vfs-crypto-timers.md](./captured-effects-vfs-crypto-timers.md) — captured-effect surfaces
- [node-compat-report.md](./node-compat-report.md) — generated Node.js core-test pass/fail report

## Repo-only engineering notes

Design records for contributors — kept here for history and rationale, not
built into the site. Status headers inside each file are authoritative;
several document retired or superseded work:

- [interpreter-optimization.md](./interpreter-optimization.md) — measured optimization phases
- [js-performance-roadmap.md](./js-performance-roadmap.md) — profiling data and roadmap
- [js-object-shapes-design.md](./js-object-shapes-design.md) — hidden-class design (implemented)
- [jit.md](./jit.md) — closure-threading JIT experiment (**retired**; kept as data)
- [cranelift-jit.md](./cranelift-jit.md) — Cranelift kernel JIT (**experimental**, opt-in `jit` feature / `chidori-js-jit` binary)
- [os-isolation-plan.md](./os-isolation-plan.md) — process isolation design
- [resume-performance.md](./resume-performance.md) — resume cost analysis
- [dom-runtime-prototype.md](./dom-runtime-prototype.md) — DOM runtime prototype
- [ai-sdk-gap-analysis.md](./ai-sdk-gap-analysis.md) — feature comparison vs Vercel AI SDK
- [rust-style-guide.md](./rust-style-guide.md) — contributor conventions
- [releasing.md](./releasing.md) — release train and versioning

Six rounds of hands-on usability reviews that shaped the developer
experience (each pinned to the version it reviewed — findings may be fixed
in later releases):

- [consumer-usability-review.md](./consumer-usability-review.md) — round 1: building a real agent (linear path)
- [consumer-usability-review-2.md](./consumer-usability-review-2.md) — round 2: the multi-agent surface (actors, branches, detached agents) under failure
- [consumer-usability-review-3.md](./consumer-usability-review-3.md) — round 3: the everyday-agent surface as a daily driver
- [consumer-usability-review-4.md](./consumer-usability-review-4.md) — round 4: the day-2 surface (npm packages, durable store, hydration, time travel, `verify`)
- [consumer-usability-review-5.md](./consumer-usability-review-5.md) — round 5: shipping to users (`serve` in production posture, SSE streaming, multiplayer signals under crashes, SDK-as-client, webhooks)
- [consumer-usability-review-6.md](./consumer-usability-review-6.md) — round 6: the long-haul conversational surface (`init`/`chat` funnel, templates, cross-run memory, window compaction, local prompt cache)

## Posts

Longer-form writing about the ideas behind the framework, under
[posts/](./posts/) ([posts/meta.json](./posts/meta.json) lists the published
set; `posts/harness-engineering-thread.md` is repo-only).
