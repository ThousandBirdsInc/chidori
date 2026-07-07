// Actors: spawn two supervised worker processes, farm tasks out to them over
// message passing, collect the replies, then settle both and fold their call
// histories into this run's durable log. Run with:
//
//   chidori run examples/agents/actor_pipeline.ts
//
// No LLM calls — this demonstrates the process model (docs/actors.md).
import { chidori, run } from "chidori:agent";

run(async () => {
  const workers = [
    await chidori.spawnActor("actors/worker.ts", { id: 1 }, { name: "worker-1" }),
    await chidori.spawnActor("actors/worker.ts", { id: 2 }, { name: "worker-2" }),
  ];

  // Wait for both workers to come up.
  await chidori.receive("ready");
  await chidori.receive("ready");

  // Round-robin the tasks and collect every reply.
  const tasks = [1, 2, 3, 4, 5, 6];
  for (const [i, n] of tasks.entries()) {
    await chidori.sendActor(workers[i % workers.length].pid, "task", { n });
  }
  const results: { id: number; n: number; square: number }[] = [];
  while (results.length < tasks.length) {
    const reply = await chidori.receive("result");
    results.push(reply.payload as { id: number; n: number; square: number });
  }

  // Tell the workers to finish and settle them.
  const summaries = [];
  for (const worker of workers) {
    await chidori.sendActor(worker.pid, "finish", null);
    const outcome = await chidori.joinActor(worker.pid);
    summaries.push(outcome.output);
  }

  results.sort((a, b) => a.n - b.n);
  return { results, summaries };
});
