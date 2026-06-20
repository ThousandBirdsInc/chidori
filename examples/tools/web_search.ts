import type { Chidori, ToolDefinition } from "chidori:agent";

export const tool: ToolDefinition = {
  name: "web_search",
  description: "Search the web for a short query.",
  parameters: {
    type: "object",
    properties: {
      query: { type: "string", description: "Search query" },
    },
    required: ["query"],
  },
};

export async function run(args: { query: string }, chidori: Chidori) {
  await chidori.log("Running web_search", { query: args.query });
  return {
    query: args.query,
    results: [],
  };
}
