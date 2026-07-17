# Release-Notes Concierge — a Chidori consumer demo

A durable release-notes desk for a real repository — the demo built for
[consumer usability review, round 3](../../docs/consumer-usability-review-3.md).
It exercises the "everyday agent" surface of Chidori on a non-default
provider (DeepSeek):

- `chidori.step` — memoized parse of a 90KB `git log --numstat` dump
- `chidori.prompt(..., { format: "json" })` — structured theme clustering
- the built-in provider tool loop (`tools` + `maxTurns`) over two local tools
  (`commit_detail`, `search_commits`) that read the dump via `workspace.read`
- `chidori.conversation()` — an editorial revise-until-approved dialogue
- `chidori.memory` — house style learned in one session is applied in the next
- `chidori.input()` — human feedback gate (scripted or interactive)
- `chidori.workspace.write` — publishes `RELEASE_NOTES.md`

Two gotchas this demo already works around (see the review for details):
shared helper modules must live *inside* `tools/` (an import that escapes
the tool directory silently unregisters the tool), and reasoning models
need a much larger `maxTokens` than the visible output suggests (hidden
reasoning spends the same budget — the guard after the clustering prompt
fails loudly instead of publishing an empty document).

## Setup

`data/gitlog.txt` ships with a 50-commit snapshot of this repository. To
point the desk at another repo (or refresh the window):

```bash
git -C /path/to/repo log --date=short --numstat \
  --pretty=format:'COMMIT %h%nDATE %ad%nSUBJECT %s%nBODY%n%b%nFILES' -60 \
  > data/gitlog.txt
```

```bash

export CHIDORI_OPENAI_COMPAT_URL=https://api.deepseek.com
export CHIDORI_OPENAI_COMPAT_KEY=sk-...
```

## Run

```bash
# scripted feedback (deterministic — replayable as a $0 regression test)
chidori run agent.ts --trusted --tools tools --model deepseek-v4-flash \
  --input '{"feedback": ["Tighten the intro.", "approve"]}'

# interactive: answer the feedback prompt at the terminal
chidori run agent.ts --trusted --tools tools --model deepseek-v4-flash
```

Editor types: `npm install` (dev-dep on `@1kbirds/chidori`), `npx tsc`.
