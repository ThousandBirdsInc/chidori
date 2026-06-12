import type { BranchOutcome, Chidori } from "chidori";

/**
 * Branching example (docs/branching-execution.md).
 *
 * The agent does shared "expensive" prefix work once, then calls
 * `chidori.branch` to fork into two strategy modules from that anchored state.
 * Each branch runs its OWN source file (editable independently under
 * `strategies/`), receives the prefix's result as explicit `input`, and
 * returns an outcome. The agent compares the outcomes and picks one — the fork
 * is a controlled experiment: the shared prefix is identical, so the only
 * variable is each branch's code.
 *
 * Durability: the whole fan-out is ONE recorded `branch` call. Replaying this
 * run (`chidori replay <run-id>`) returns the outcomes from the call log
 * without re-running either branch.
 *
 * `summarizeBrief` is a local helper so the example runs offline with no LLM
 * provider; swap it (and the strategies) for `chidori.prompt(...)` calls to
 * see real model spans nested under each branch subtree in the trace.
 */

type Brief = { topic?: string };

export async function agent(input: Brief, chidori: Chidori) {
  const topic = input.topic ?? "incident postmortem";

  // Shared prefix: paid once, handed to every branch as state.
  await chidori.log(`researching: ${topic}`);
  const research = summarizeBrief(topic);

  const outcomes = await chidori.branch([
    {
      label: "outline-first",
      source: "examples/branching/strategies/outline_first.ts",
      input: { topic, research },
    },
    {
      label: "draft-direct",
      source: "examples/branching/strategies/draft_direct.ts",
      input: { topic, research },
    },
  ]);

  for (const outcome of outcomes) {
    await chidori.log(
      `branch ${outcome.label}: ${outcome.status}` +
        (outcome.status === "failed" ? ` (${outcome.error})` : ""),
    );
  }

  // Compare and pick: here, the longest completed draft wins.
  const completed = outcomes.filter((o) => o.status === "completed");
  const best = completed.reduce((a, b) => (score(a) >= score(b) ? a : b));
  await chidori.log(`picked: ${best.label}`);

  return { picked: best.label, draft: best.output, outcomes };
}

function score(outcome: BranchOutcome): number {
  const draft = (outcome.output as { draft?: string } | undefined)?.draft ?? "";
  return draft.length;
}

function summarizeBrief(topic: string): string {
  return `key facts about ${topic}: timeline reconstructed; root cause identified; two follow-ups proposed`;
}
