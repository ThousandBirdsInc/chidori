import type { Chidori, ToolDefinition } from "chidori:agent";

// A deliberately flaky tool, deterministic on the attempt number the agent
// passes in: the first two attempts report failure, the third succeeds. The
// agent's retry loop drives it. Recording captures the whole 503,503,200 path so
// a replay reproduces the exact history without touching the service.
//
// Failure is signalled by a returned `{ ok: false }` flag rather than a thrown
// exception: the call log records the tool's *return value*, so a return-based
// failure replays exactly. (A thrown error would also be recorded, but rejected
// effects do not currently re-reject cleanly on replay — return-flag failures
// are the replay-safe idiom.)
export const tool: ToolDefinition = {
  name: "flaky_fetch",
  description: "Fetch a config value from a flaky upstream service.",
  parameters: {
    type: "object",
    properties: {
      key: { type: "string", description: "Config key to fetch" },
      attempt: { type: "integer", description: "1-based attempt number" },
    },
    required: ["key", "attempt"],
  },
};

export async function run(args: { key: string; attempt: number }, chidori: Chidori) {
  await chidori.log("flaky_fetch attempt", { key: args.key, attempt: args.attempt });
  if (args.attempt < 3) {
    return { ok: false, status: 503, error: "Service Unavailable" };
  }
  return { ok: true, status: 200, value: { flag: true }, servedOnAttempt: args.attempt };
}
