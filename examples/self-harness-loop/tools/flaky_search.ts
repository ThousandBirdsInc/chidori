import type { ToolDefinition } from "chidori:agent";

/**
 * The seeded failure for the self-harness-loop demo: a search tool with a
 * simulated transient fault. The FIRST attempt at any query times out; a
 * retry (attempt >= 2) succeeds. Deterministic on its arguments, so runs
 * replay byte-identically and the naive-vs-retry branch comparison is a
 * controlled experiment.
 */
export const tool: ToolDefinition = {
  name: "flaky_search",
  description:
    "Search the knowledge base. Transiently flaky: first attempts time out; pass a higher `attempt` to retry.",
  parameters: {
    type: "object",
    properties: {
      query: { type: "string", description: "The search query" },
      attempt: {
        type: "number",
        description: "Retry counter, starting at 1",
      },
    },
    required: ["query"],
  },
};

export async function run(args: { query: string; attempt?: number }) {
  const attempt = args.attempt ?? 1;
  if (attempt < 2) {
    // The transient fault: identical to a real search backend shedding load.
    throw new Error(
      `flaky_search: upstream timeout after 5000ms (attempt ${attempt})`,
    );
  }
  return {
    source: "knowledge-base",
    results: [
      {
        title: `Results for "${args.query}"`,
        snippet: `Everything the worker needs to know about ${args.query}.`,
      },
    ],
  };
}
