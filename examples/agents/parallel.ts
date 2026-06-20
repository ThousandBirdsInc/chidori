import type { Chidori } from "chidori:agent";

export async function agent(input: { topic: string }, chidori: Chidori) {
  const drafts = await chidori.parallel(
    [
      async () =>
        chidori.prompt("List two implementation risks for: " + input.topic, {
          type: "draft",
          maxTokens: 120,
        }),
      async () =>
        chidori.prompt("List two user-facing progress updates for: " + input.topic, {
          type: "draft",
          maxTokens: 120,
        }),
    ],
    { concurrency: 2 },
  );

  return { drafts };
}
