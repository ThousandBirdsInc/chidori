/// <reference types="@1kbirds/chidori/agent-env" />
import { chidori, run } from "chidori:agent";

/**
 * Ask the scribe — a conversational companion over everything the Standup
 * Scribe has accumulated: the thread ledger in `chidori.memory` and the
 * published briefs in the workspace.
 *
 * Interactive REPL:
 *   chidori chat ask.ts --model deepseek-v4-flash
 *
 * One-shot / driven:
 *   chidori run ask.ts --input '{"messages": ["What is blocked right now?"]}'
 */

type Thread = { id: string; title: string; owner: string; status: string; note: string };

run(async (input: { messages?: string[]; model?: string }) => {
  const threads = ((await chidori.memory.get("threads")) ?? []) as Thread[];
  const lastWeek = ((await chidori.memory.get("lastWeek")) ?? null) as string | null;

  const briefs: string[] = [];
  const entries = await chidori.workspace.list();
  for (const e of entries) {
    if (e.path.startsWith("briefs/") && e.path.endsWith(".md")) {
      briefs.push(await chidori.workspace.read(e.path));
    }
  }

  if (threads.length === 0 && briefs.length === 0) {
    return {
      transcript: [
        {
          role: "assistant",
          text: "I have no memory of this team yet — run the scribe first: chidori run agent.ts --input week=week1",
        },
      ],
    };
  }

  const chat = chidori.conversation({
    system:
      "You are the Kestrel team's standup scribe, answering questions about " +
      "the team's recent work. Ground every answer in the thread ledger and " +
      "weekly briefs below; if they don't cover it, say so.\n\n" +
      `Most recent week digested: ${lastWeek ?? "unknown"}\n\n` +
      "Thread ledger:\n" +
      JSON.stringify(threads, null, 2) +
      "\n\nWeekly briefs:\n" +
      briefs.join("\n\n---\n\n"),
    model: input.model,
    maxTokens: 1200,
  });

  const messages = input.messages ?? [];
  if (messages.length > 0) {
    for (const message of messages) await chat.say(message);
    return { transcript: chat.history() };
  }

  const transcript = await chat.loop({ prompt: "ask the scribe>" });
  return { transcript };
});
