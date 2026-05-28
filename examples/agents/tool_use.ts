import type { Chidori } from "chidori";

export async function agent(input: { query: string }, chidori: Chidori) {
  const result = await chidori.tool("web_search", { query: input.query });
  return { result };
}
