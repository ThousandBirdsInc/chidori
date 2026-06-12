// Value checkpoints (`chidori.step`) — bound resume cost on long runs.
//
// Resume re-executes the agent from the top, serving recorded host effects
// from the journal. Pure JS compute is normally re-executed on every replay;
// wrapping it in `chidori.step(name, fn)` journals the result once, so a
// resumed or replayed run returns the recorded value without re-running fn.
//
//   chidori run examples/agents/value_checkpoint.ts --input '{"docs": 2000}'
//
// The run pauses on `input()`; resume it and the "index" step is served from
// the journal instead of being recomputed. See docs/value-checkpoints.md.
import type { Chidori } from "chidori";

export async function agent(input: { docs?: number }, chidori: Chidori) {
  const docs = input.docs ?? 1000;

  // Expensive, deterministic, effect-free — exactly what a step is for.
  // Host effects, randomness, fs writes, and timers throw inside the
  // callback: a skipped body must have nothing to skip but compute.
  const index = await chidori.step("index", () => {
    const buckets: Record<string, number> = {};
    for (let i = 0; i < docs; i++) {
      const shard = `shard-${i % 16}`;
      buckets[shard] = (buckets[shard] ?? 0) + 1;
    }
    return { docs, shards: Object.keys(buckets).length, buckets };
  });

  await chidori.log("index built", { docs: index.docs, shards: index.shards });

  // The pause that makes the checkpoint pay off: resuming this run replays
  // the journal — the step record answers instantly, the loop never re-runs.
  const answer = await chidori.input("Publish the index?");

  return { published: answer === "yes", docs: index.docs, shards: index.shards };
}
