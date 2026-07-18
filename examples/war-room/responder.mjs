// A war-room responder: delivers a signal to a live incident session.
//
// Usage:
//   node responder.mjs <sessionId> note "redis maxclients doubled last week" --as dana
//   node responder.mjs <sessionId> escalate --as marcus
//   node responder.mjs <sessionId> approve mitigate --as sam
//   node responder.mjs <sessionId> approve abandon --as sam
import { AgentClient, isSignalQueued } from "@1kbirds/chidori";

const [sessionId, name, ...rest] = process.argv.slice(2);
const asIdx = rest.indexOf("--as");
const who = asIdx >= 0 ? rest[asIdx + 1] : "anonymous";
const args = asIdx >= 0 ? rest.slice(0, asIdx) : rest;

if (!sessionId || !name) {
  console.error("usage: node responder.mjs <sessionId> <approve|note|escalate> [text|decision] --as <id>");
  process.exit(2);
}

const payload =
  name === "note" ? { text: args.join(" ") } :
  name === "approve" ? { decision: args[0] ?? "mitigate" } :
  {};

const client = new AgentClient(process.env.WARROOM_URL ?? "http://127.0.0.1:8787", {
  apiKey: process.env.WARROOM_API_KEY,
});
const result = await client.signal(sessionId, {
  name,
  payload,
  from: { kind: "human", id: who },
});

if (isSignalQueued(result)) {
  console.log(`[${who}] ${name} ${result.status} (delivery_seq ${result.delivery_seq})`);
} else {
  console.log(`[${who}] ${name} resolved a pause; session now ${result.status}`);
}
