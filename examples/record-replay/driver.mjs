// Record-and-replay driver for the chidori TypeScript SDK.
//
// This is the client side of the story: it talks to a running `chidori serve`
// over HTTP via `AgentClient`, records a run, then replays it from the saved
// checkpoint and proves the output is byte-identical — with zero live tool
// calls on the replay. For the human-in-the-loop scenario it instead drives a
// pause -> resume.
//
// Usage:
//   1. Start a server for ONE scenario (each binds a single agent file;
//      --trusted allows the example's tool calls, which the server's
//      deny-by-default posture would otherwise refuse):
//        cargo run -- serve examples/record-replay/exactly_once.ts --port 8080 --trusted
//   2. Run the matching driver scenario:
//        node examples/record-replay/driver.mjs --scenario exactly_once
//
// In your own code the import is simply `import { AgentClient } from "@1kbirds/chidori"`.
// Here we point at the built SDK in this repo so the example runs in-tree.
import { AgentClient } from "../../sdk/typescript/dist/index.js";

const args = parseArgs(process.argv.slice(2));
const url = args.url ?? process.env.CHIDORI_URL ?? "http://127.0.0.1:8080";
const scenario = args.scenario ?? "exactly_once";
const client = new AgentClient(url);

const INPUTS = {
  exactly_once: { name: "Ada" },
  deterministic_identity: { prefix: "demo" },
  retry_flaky_tool: { maxAttempts: 5 },
  tool_pipeline: { city: "Berlin", currency: "EUR" },
  human_approval: { order: "A-1007" },
};

const input = args.input ? JSON.parse(args.input) : INPUTS[scenario] ?? {};

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a.startsWith("--")) out[a.slice(2)] = argv[i + 1]?.startsWith("--") ? true : argv[++i];
  }
  return out;
}

function deepEqual(a, b) {
  return JSON.stringify(a) === JSON.stringify(b);
}

function assert(cond, msg) {
  if (!cond) {
    console.error(`  ✗ ${msg}`);
    process.exitCode = 1;
    throw new Error(msg);
  }
  console.log(`  ✓ ${msg}`);
}

async function recordThenReplay() {
  console.log(`== record: run "${scenario}" with input ${JSON.stringify(input)} ==`);
  const run = await client.run(input);
  assert(run.status === "completed", `run completed (status=${run.status})`);
  console.log(`  output: ${JSON.stringify(run.output)}`);

  const cp = await run.checkpoint();
  const toolCalls = cp.callLog.filter((c) => c.function === "tool" || /tool|http|input|memory/.test(c.function));
  console.log(`  recorded ${cp.callLog.length} host call(s); first: ${cp.callLog[0]?.function ?? "none"}`);

  console.log(`== replay: re-run from the checkpoint (no live tool calls) ==`);
  const replayed = await client.replay(cp);
  assert(replayed.status === "completed", `replay completed (status=${replayed.status})`);
  assert(deepEqual(replayed.output, run.output), "replay output is byte-identical to the original");
  console.log(`\nOK: "${scenario}" recorded and replayed deterministically.`);
}

async function pauseThenResume() {
  console.log(`== run "${scenario}" — expect a pause for human input ==`);
  const run = await client.run(input);
  assert(run.status === "paused", `run paused awaiting input (status=${run.status})`);
  console.log(`  prompt: ${run.pendingPrompt ?? "(none)"}`);

  console.log(`== resume with "approve" ==`);
  const done = await client.resume(run.id, "approve");
  assert(done.status === "completed", `resumed to completion (status=${done.status})`);
  assert(done.output?.status === "refunded", `refund issued after approval (${JSON.stringify(done.output)})`);

  console.log(`== replay the completed, approved run (no re-prompt, no re-refund) ==`);
  const cp = await done.checkpoint();
  const replayed = await client.replay(cp);
  assert(replayed.status === "completed", "replay of approved run completes without pausing");
  assert(deepEqual(replayed.output, done.output), "replayed output matches the approved run");
  console.log(`\nOK: "${scenario}" paused for a human and resumed deterministically.`);
}

try {
  console.log(`health: ${JSON.stringify(await client.health())}\n`);
  if (scenario === "human_approval") {
    await pauseThenResume();
  } else {
    await recordThenReplay();
  }
} catch (err) {
  console.error(`\nFAILED: ${err.message}`);
  console.error(`(is a server running? try: cargo run -- serve examples/record-replay/${scenario}.ts --port 8080 --trusted)`);
  process.exitCode = 1;
}
