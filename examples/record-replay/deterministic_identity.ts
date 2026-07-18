import { chidori, run } from "chidori:agent";

/**
 * Deterministic non-determinism — reproducible ids, clocks, and choices.
 *
 * Agents reach for non-deterministic primitives constantly: a fresh id, the
 * current time, a sampled branch. Two runtime facts make them reproducible:
 *
 *   1. Policy knobs (`date: "fixed"`, `random: "seeded"`) make Date.now() and
 *      Math.random() deterministic, so the same code yields the same values on
 *      every replay.
 *   2. `chidori.step(name, fn)` records the computed value into the durable
 *      call log and serves it verbatim on replay — so the minted id is obvious
 *      in `chidori trace` and never recomputed on resume.
 *
 * Replay sees exactly what the original run saw — reproducible for audit or
 * time-travel debugging.
 */
run(async (input: { prefix?: string }) => {
  const prefix = input.prefix ?? "run";

  const minted = await chidori.step("mint_id", () => {
    const epochMs = Date.now();
    const rand = Math.floor(Math.random() * 1e9).toString(36);
    return { id: `${prefix}-${epochMs}-${rand}`, epochMs };
  });

  // Derive a deterministic branch from the recorded id so the choice replays too.
  const lane = minted.epochMs % 2 === 0 ? "fast" : "slow";

  await chidori.memory.set("identity", { id: minted.id, lane });

  return { runId: minted.id, startedAt: minted.epochMs, lane };
});
