// Smoke test: run + replay + stream against a locally running server.
import { AgentClient } from "./dist/index.js";

const client = new AgentClient("http://127.0.0.1:8767");

console.log("health:", await client.health());

const run = await client.run({ user: "grace", theme: "emerald" });
console.log("run status:", run.status);
console.log("run output:", run.output);

const cp = await run.checkpoint();
console.log("call log length:", cp.callLog.length);
console.log("first call:", cp.callLog[0]?.function);

const replayed = await client.replay(cp);
console.log("replay status:", replayed.status);
console.log("replay output equal:", JSON.stringify(replayed.output) === JSON.stringify(run.output));

console.log("--- stream ---");
for await (const evt of client.stream({ user: "heidi", theme: "coal" })) {
  if (evt.type === "call") console.log("call:", evt.record.function, "seq", evt.record.seq);
  if (evt.type === "done") console.log("done:", evt.status);
}
