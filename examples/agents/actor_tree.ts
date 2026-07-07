// Supervision tree: the run spawns two supervisor actors, each of which owns
// its own worker pool and restart policy. Messages, joins, and restarts stay
// within each subtree; the run only talks to the supervisors. Run with:
//
//   chidori run examples/agents/actor_tree.ts
//
// No LLM calls — this demonstrates the tree model (docs/actors.md).
import { chidori, run } from "chidori:agent";

run(async () => {
  const shards = [
    await chidori.spawnActor("actors/supervisor.ts", { tasks: [1, 2, 3], workers: 2 }),
    await chidori.spawnActor("actors/supervisor.ts", { tasks: [4, 5, 6], workers: 2 }),
  ];
  const outcomes = [];
  for (const shard of shards) {
    outcomes.push(await chidori.joinActor(shard.pid));
  }
  return {
    statuses: outcomes.map((o) => o.status),
    shards: outcomes.map((o) => o.output),
  };
});
