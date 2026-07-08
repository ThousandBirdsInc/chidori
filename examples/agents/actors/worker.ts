// A worker actor: announces itself, processes tasks sent by its spawner until
// it is told to finish, then returns a summary of everything it handled.
// Spawned by ../actor_pipeline.ts and actors/supervisor.ts — see docs/actors.md.
import { chidori, run } from "chidori:agent";

run(async (input: { id: number }) => {
  await chidori.actors.send("parent", "ready", { id: input.id });
  const handled: number[] = [];
  for (;;) {
    const msg = await chidori.receive(["task", "finish"]);
    if (msg.timedOut || msg.name === "finish") break;
    const task = msg.payload as { n: number };
    handled.push(task.n);
    await chidori.actors.send("parent", "result", {
      id: input.id,
      n: task.n,
      square: task.n * task.n,
    });
  }
  return { id: input.id, handled };
});
