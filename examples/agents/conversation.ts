import type { Chidori } from "chidori:agent";

/**
 * A multi-turn conversational agent — the "chat assistant" shape — built on
 * `chidori.conversation()`.
 *
 * `conversation()` owns the running context: the system prompt is frozen once
 * as a cacheable prefix, and every `chat.say(message)` appends the user turn,
 * makes one durable `prompt` host call, and threads the assistant turn back in
 * for the next message. So the whole dialogue is recorded and replays for $0,
 * and each turn after the first reads the shared prefix at the cached rate.
 *
 * Two ways to drive it:
 *
 *   Scripted (deterministic — used by `--input` and replay tests):
 *     chidori run examples/agents/conversation.ts \
 *       --input '{"messages": ["Hi, who are you?", "What can you help with?"]}'
 *
 *   Interactive (reads each turn from the terminal, or from a paused session
 *   under `chidori serve`; type "exit" or "quit" to end):
 *     chidori run examples/agents/conversation.ts
 */
export async function agent(
  input: { system?: string; messages?: string[] },
  chidori: Chidori,
) {
  const chat = chidori.conversation({
    system:
      input.system ??
      "You are a concise, friendly assistant. Keep replies to a sentence or two.",
    // Opt-in window management: a pure no-op until the running tail exceeds the
    // budget, then the older turns fold into one recorded summary segment.
    compact: { budgetTokens: 8000 },
  });

  // Scripted mode: a fixed list of user turns. Deterministic, so it replays
  // byte-for-byte with zero LLM calls — check a transcript in as a test.
  if (Array.isArray(input.messages) && input.messages.length > 0) {
    for (const message of input.messages) {
      const reply = await chat.say(message);
      await chidori.log("turn", { user: message, reply });
    }
    return { turns: chat.length, transcript: chat.history() };
  }

  // Interactive mode: read each human message via chidori.input(). Under
  // `chidori run` this blocks on stdin; under `chidori serve` it pauses the
  // session to disk and resumes on the next POST /sessions/{id}/resume.
  const transcript = await chat.loop({
    prompt: "you>",
    onReply: (reply) => chidori.log("assistant", { reply }),
  });

  return { turns: chat.length, transcript };
}
