// A mid-tree supervisor: spawned by ../actor_tree.ts, it owns its own worker
// pool (a supervision tree — see docs/actors.md). The workers' "parent"
// messages route HERE, to the owning actor, not to the run; and only this
// supervisor may join the workers it spawned.
import { chidori, run } from "chidori:agent";

run(async (input: { tasks: number[]; workers: number }) => {
  const pool = [];
  for (let id = 1; id <= input.workers; id += 1) {
    // Paths resolve against the run entrypoint's directory (the project
    // root), not this module's own directory.
    pool.push(
      await chidori.actors.spawn("actors/worker.ts", { id }, { restart: "clean", maxRestarts: 1 }),
    );
  }
  for (let i = 0; i < input.workers; i += 1) {
    await chidori.receive("ready");
  }

  for (const [i, n] of input.tasks.entries()) {
    await pool[i % pool.length].send("task", { n });
  }
  const results = [];
  while (results.length < input.tasks.length) {
    const reply = await chidori.receive("result");
    results.push(reply.payload as { id: number; n: number; square: number });
  }

  const summaries = [];
  for (const worker of pool) {
    await worker.send("finish", null);
    summaries.push((await worker.join()).output ?? null);
  }
  results.sort((a, b) => a.n - b.n);
  return { results, summaries };
});
