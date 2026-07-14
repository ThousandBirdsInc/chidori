import { chidori, run } from "chidori:agent";

run(async (input: { topic: string }) => {
  const drafts = await chidori.util.parallel(
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
});
