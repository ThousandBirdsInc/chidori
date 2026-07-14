import { chidori, run } from "chidori:agent";

run(async (input: { topic: string }) => {
  const note = await chidori.prompt(
    "As a nested sub-agent, explain why labelled prompt streams matter for: " +
      input.topic,
    { type: "subagent", maxTokens: 120 },
  );
  return { note };
});
