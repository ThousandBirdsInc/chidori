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
    await chidori.actors.spawn("actors/worker.ts", { id: 1 }),
    await chidori.actors.spawn("actors/worker.ts", { id: 2 }),
  ];

  // Wait for both workers to come up.
  await chidori.receive("ready");
  await chidori.receive("ready");

  // Round-robin the tasks and collect every reply.
  const tasks = [1, 2, 3, 4, 5, 6];
  for (const [i, n] of tasks.entries()) {
    await workers[i % workers.length].send("task", { n });
  }
  const results: { id: number; n: number; square: number }[] = [];
  while (results.length < tasks.length) {
    const reply = await chidori.receive("result");
    results.push(reply.payload as { id: number; n: number; square: number });
  }

  // Tell the workers to finish and settle them.
  const summaries = [];
  for (const worker of workers) {
    await worker.send("finish", null);
    const outcome = await worker.join();
    summaries.push(outcome.output);
  }

  results.sort((a, b) => a.n - b.n);
  return { results, summaries };
});
