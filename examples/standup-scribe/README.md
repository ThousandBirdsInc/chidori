# Standup Scribe

A consumer demo built for
[consumer usability review, round 6](../../docs/consumer-usability-review-6.md):
the **long-haul conversational surface** — templates, cross-run memory,
explicit window compaction, and the `chidori chat` REPL.

The scribe lives with a team for weeks. Each week it digests the raw standup
transcripts under `data/<week>/` in one running conversation:

1. **`chidori.template`** renders every prompt from the `prompts/*.jinja`
   files — no string concatenation in the agent.
2. **`Context.compact({ budgetTokens })`** folds older days into a recorded
   summary segment once the conversation outgrows its budget, so a month of
   standups never overflows the window — and the compaction itself replays
   deterministically.
3. **`chidori.memory`** holds the thread ledger between runs: week 2 starts
   from the open threads week 1 left behind.
4. **`chidori.input`** shows the drafted brief for approval before
   **`chidori.workspace.write`** publishes it to `briefs/<week>.md`.

`ask.ts` is the companion: a conversational agent over the accumulated
ledger + briefs, driven interactively by `chidori chat`.

## Run it

```bash
export CHIDORI_OPENAI_COMPAT_URL=https://api.deepseek.com   # any OpenAI-compatible provider
export CHIDORI_OPENAI_COMPAT_KEY=sk-...

cd examples/standup-scribe
chidori run agent.ts --input week=week1 --model deepseek-v4-flash --trusted
chidori run agent.ts --input week=week2 --model deepseek-v4-flash --trusted   # carries week-1 threads

chidori chat ask.ts --model deepseek-v4-flash                # "what's blocked right now?"
```

Replay either week byte-for-byte for $0, or check it in as a CI fixture:

```bash
chidori resume agent.ts <run_id>
chidori verify agent.ts <run_id>
```
