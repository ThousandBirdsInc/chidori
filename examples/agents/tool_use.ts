import { chidori, run } from "chidori:agent";

run(async (input: { query: string }) => {
  const result = await chidori.tool("web_search", { query: input.query });
  return { result };
});
