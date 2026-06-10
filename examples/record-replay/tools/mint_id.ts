import type { Chidori, ToolDefinition } from "chidori";

// Non-deterministic by nature: a fresh id + wall-clock reading. Because the
// result is recorded in the call log, every replay reproduces the SAME id and
// timestamp — so an agent run is perfectly reproducible for audit/debugging.
export const tool: ToolDefinition = {
  name: "mint_id",
  description: "Mint a unique run id stamped with the current time.",
  parameters: {
    type: "object",
    properties: {
      prefix: { type: "string", description: "Id prefix" },
    },
    required: [],
  },
};

export async function run(args: { prefix?: string }, _chidori: Chidori) {
  const prefix = args.prefix ?? "run";
  const epochMs = Date.now();
  const rand = Math.floor(Math.random() * 1e9).toString(36);
  return { id: `${prefix}-${epochMs}-${rand}`, epochMs };
}
