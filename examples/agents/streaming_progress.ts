import type { Chidori } from "chidori";

export async function agent(input: { topic: string }, chidori: Chidori) {
  const progress = await chidori.prompt(
    "In one sentence, say what work is starting for: " + input.topic,
    { type: "progress", maxTokens: 80 },
  );

  const drafts = await chidori.parallel([
    async () =>
      chidori.prompt("Draft two implementation risks for: " + input.topic, {
        type: "draft",
        maxTokens: 120,
      }),
    async () =>
      chidori.prompt("Draft two progress updates for: " + input.topic, {
        type: "draft",
        maxTokens: 120,
      }),
  ]);

  const subagent = await chidori.callAgent("examples/agents/streaming_progress_child.ts", {
    topic: input.topic,
  });

  const final = await chidori.prompt(
    "Write a concise final answer for a product user.\n\n" +
      "Topic: " +
      input.topic +
      "\nProgress: " +
      progress +
      "\nDrafts: " +
      JSON.stringify(drafts) +
      "\nSub-agent: " +
      JSON.stringify(subagent),
    { type: "final", maxTokens: 220 },
  );

  return { progress, drafts, subagent, final };
}
