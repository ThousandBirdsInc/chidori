import type { Chidori } from "chidori";

/**
 * Deterministic non-determinism — reproducible ids, clocks, and choices.
 *
 * Agents reach for non-deterministic primitives constantly: a fresh id, the
 * current time, a sampled branch. The `mint_id` tool produces both an id and a
 * timestamp; because the tool call is recorded, every replay reproduces the
 * SAME id and timestamp. That makes an agent run reproducible for audit or
 * time-travel debugging — replay sees exactly what the original run saw.
 *
 * (The Chidori runtime also has policy knobs — `date: "fixed"`,
 * `random: "seeded"` — that make Date.now()/Math.random() deterministic without
 * a tool. This example uses an explicit tool so the recorded value is obvious in
 * the call log.)
 */
export async function agent(input: { prefix?: string }, chidori: Chidori) {
  const minted = await chidori.tool<{ prefix: string }, { id: string; epochMs: number }>("mint_id", {
    prefix: input.prefix ?? "run",
  });

  // Derive a deterministic branch from the recorded id so the choice replays too.
  const lane = minted.epochMs % 2 === 0 ? "fast" : "slow";

  await chidori.memory("set", "identity", { id: minted.id, lane });

  return { runId: minted.id, startedAt: minted.epochMs, lane };
}
