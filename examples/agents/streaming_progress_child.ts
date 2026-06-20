import type { Chidori } from "chidori:agent";

export async function agent(input: { topic: string }, chidori: Chidori) {
  const note = await chidori.prompt(
    "As a nested sub-agent, explain why labelled prompt streams matter for: " +
      input.topic,
    { type: "subagent", maxTokens: 120 },
  );
  return { note };
}
