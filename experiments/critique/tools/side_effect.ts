// A tool with an observable side effect: every real invocation appends a line
// to an on-disk ledger inside the workspace. Used by exactly_once_probe.ts to
// verify that replay does NOT re-execute the tool (the ledger length is the
// ground truth for how many times the effect actually ran).
import type { Chidori, ToolDefinition } from "chidori:agent";

export const tool: ToolDefinition = {
  name: "side_effect",
  description: "Append a line to ledger.txt and return the invocation count.",
  parameters: {
    type: "object",
    properties: {
      label: { type: "string", description: "Label recorded in the ledger" },
    },
    required: ["label"],
  },
};

export async function run(args: { label: string }, chidori: Chidori) {
  let existing = "";
  try {
    existing = String(await chidori.workspace.read("ledger.txt"));
  } catch {
    existing = "";
  }
  const updated = existing + `invoked:${args.label}\n`;
  await chidori.workspace.write("ledger.txt", updated);
  return { invocations: updated.split("\n").filter(Boolean).length };
}
