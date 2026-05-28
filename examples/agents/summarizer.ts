import type { Chidori } from "chidori";

export async function agent(input: { document: string }, chidori: Chidori) {
  const summary = await chidori.prompt(
    "Summarize this document in three concise bullets:\n\n" + input.document,
    { type: "final", maxTokens: 220 },
  );
  return { summary };
}
