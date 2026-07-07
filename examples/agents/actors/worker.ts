// A worker actor: announces itself, processes tasks sent by the parent until
// it is told to finish, then returns a summary of everything it handled.
// Spawned by ../actor_pipeline.ts — see docs/actors.md.
import { chidori, run } from "chidori:agent";

run(async (input: { id: number }) => {
  await chidori.sendActor("parent", "ready", { id: input.id });
  const handled: number[] = [];
  for (;;) {
    const msg = await chidori.receive(["task", "finish"]);
    if ("timedOut" in msg || msg.name === "finish") break;
    const task = msg.payload as { n: number };
    handled.push(task.n);
    await chidori.sendActor("parent", "result", {
      id: input.id,
      n: task.n,
      square: task.n * task.n,
    });
  }
  return { id: input.id, handled };
});
