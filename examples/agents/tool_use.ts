import { chidori, run } from "chidori:agent";

// Calls a local TypeScript tool directly — no LLM, no network, zero setup.
// (`examples/tools/web_search.ts` shows a real network-backed tool; swap the
// name and args to try it — its fetch is a gated effect like any other.)
run(async (input: { query: string }) => {
  const result = await chidori.tool("reverse", { text: input.query });
  return { result };
});
