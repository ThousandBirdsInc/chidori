import { chidori, run } from "chidori:agent";

run(async (input: { document: string }) => {
  const summary = await chidori.prompt(
    "Summarize this document in three concise bullets:\n\n" + input.document,
    { type: "final", maxTokens: 220 },
  );
  return { summary };
});
