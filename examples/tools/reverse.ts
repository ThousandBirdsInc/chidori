import type { ToolDefinition } from "chidori";

/**
 * A sample tool for the worker agent. Reverses a string. Replace the body (and
 * the schema) with whatever your agent needs — an API call, a DB query, a
 * computation. Every `chidori.tool(...)` call is policy-gated and recorded.
 */
export const tool: ToolDefinition = {
  name: "reverse",
  description: "Reverse a string and return it. A sample tool — replace with your own.",
  parameters: {
    type: "object",
    properties: {
      text: { type: "string", description: "The text to reverse" },
    },
    required: ["text"],
  },
};

export async function run(args: { text: string }) {
  return { reversed: [...String(args.text)].reverse().join("") };
}
