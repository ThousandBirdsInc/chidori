# Release-Notes Concierge — a Chidori consumer demo

A durable release-notes desk for a real repository — the demo built for
[consumer usability review, round 3](../../docs/consumer-usability-review-3.md).
It exercises the "everyday agent" surface of Chidori on a non-default
provider (DeepSeek):

- **import-defined tools** — `defineTool()` handles declared right in
  `agent.ts`, closing over the parsed commit list; no `tools/` directory,
  no `--tools` flag, no per-call re-reads. Their bodies run in the agent's
  own VM, so journaling and deterministic replay come for free
- `chidori.step` — memoized parse of a 90KB `git log --numstat` dump
- `chidori.prompt(..., { format: "json" })` — structured theme clustering
  (strict by default: a truncated reply throws instead of degrading)
- `chidori.conversation()` — an editorial revise-until-approved dialogue
- `chidori.memory` — house style learned in one session is applied in the next
- `chidori.input()` — human feedback gate (scripted or interactive)
- `chidori.workspace.write` — publishes `RELEASE_NOTES.md`

One gotcha this demo works around: reasoning models need a much larger
`maxTokens` than the visible output suggests (hidden reasoning spends the
same budget), hence the generous budgets on each prompt.

## Setup

The desk reads its input from `data/gitlog.txt` — a `git log --numstat` dump
of whatever repo you want release notes for. Generate one (from this repo, or
point `-C` at any other):

```bash
mkdir -p data
git -C /path/to/repo log --date=short --numstat \
  --pretty=format:'COMMIT %h%nDATE %ad%nSUBJECT %s%nBODY%n%b%nFILES' -60 \
  > data/gitlog.txt
```

```bash
export CHIDORI_OPENAI_COMPAT_URL=https://api.deepseek.com
export CHIDORI_OPENAI_COMPAT_KEY=sk-...
export CHIDORI_MODEL=deepseek-v4-flash
```

## Run

```bash
# scripted feedback (deterministic — replayable as a $0 regression test)
chidori run agent.ts --trusted \
  --input '{"feedback": ["Tighten the intro.", "approve"]}'

# interactive: answer the feedback prompt at the terminal
chidori run agent.ts --trusted

# verify a recorded run as a CI test: $0, milliseconds
chidori verify agent.ts <run_id>
```

Editor types: `npm install` (dev-dep on the in-repo SDK), `npx tsc`.
