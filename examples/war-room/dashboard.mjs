// War-room dashboard: opens an incident as a live streaming session and
// renders what the commander is doing in real time — prompt tokens, tool
// probes, pauses waiting on the humans — until the run finishes.
//
// Usage: node dashboard.mjs [serverUrl]
import { AgentClient, isSignalQueued } from "@1kbirds/chidori";

const serverUrl = process.argv[2] ?? "http://127.0.0.1:8787";
// WARROOM_API_KEY: the server's CHIDORI_API_KEY when auth is on (production
// posture); the SDK sends it as a bearer token on every request incl. SSE.
const client = new AgentClient(serverUrl, {
  timeoutMs: 30_000,
  apiKey: process.env.WARROOM_API_KEY,
});

const alert = {
  id: "INC-4207",
  service: "checkout-api",
  summary: "5xx spike on POST /checkout — 34% of requests failing",
  errorRate: 0.34,
  logs: [
    '2026-07-17T09:41:03Z ERROR checkout-api pool=redis-sessions "connection refused: max clients reached"',
    '2026-07-17T09:41:04Z ERROR checkout-api handler=/checkout status=502 upstream=redis-sessions',
    '2026-07-17T09:41:07Z WARN  checkout-api retry storm detected, backoff engaged',
  ].join("\n"),
};

const t0 = Date.now();
const ts = () => `+${((Date.now() - t0) / 1000).toFixed(1)}s`;
let currentType = null;

console.log(`[dash ${ts()}] opening incident ${alert.id} against ${serverUrl}`);

try {
  for await (const evt of client.stream({
    alert,
    approvalTimeoutMs: Number(process.env.WARROOM_TIMEOUT_MS ?? 45_000),
    maxPages: 2,
  })) {
    switch (evt.type) {
      case "prompt_start":
        currentType = evt.prompt_type ?? "?";
        process.stdout.write(`\n[dash ${ts()}] ── model (${currentType}) `);
        break;
      case "prompt_delta":
        process.stdout.write(evt.delta);
        break;
      case "prompt_end":
        process.stdout.write(`\n[dash ${ts()}] ── end (${currentType})\n`);
        break;
      case "call": {
        // Consumed signals carry who steered the run — render the attribution.
        const r = evt.record;
        const isSignal = r.function === "signal" || r.function === "signal_any";
        const from = isSignal && r.result?.from ? `${r.result.from.kind}:${r.result.from.id}` : null;
        console.log(
          `[dash ${ts()}] call #${r.seq} ${r.function}` +
            (from ? ` ← ${r.result.name} from ${from}: ${JSON.stringify(r.result.payload)}` : ""),
        );
        break;
      }
      case "paused":
        console.log(
          `\n[dash ${ts()}] ⏸ WAR ROOM OPEN — session ${evt.id} waiting on ` +
            `${JSON.stringify(evt.pending_signal_names ?? [evt.pending_signal_name])}` +
            (evt.pending_signal_deadline ? ` (deadline ${evt.pending_signal_deadline})` : ""),
        );
        console.log(`[dash ${ts()}]   respond with: node responder.mjs ${evt.id} <approve|note|escalate> ...`);
        break;
      case "done":
        console.log(`\n[dash ${ts()}] ✅ done: ${evt.status}`);
        console.log(JSON.stringify(evt.output ?? evt.error, null, 2));
        break;
    }
  }
} catch (err) {
  console.error(`[dash ${ts()}] stream failed:`, err.message);
  process.exit(1);
}
