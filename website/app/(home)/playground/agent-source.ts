/**
 * The chidori agent that drives the playground chat. This exact source is
 * transpiled and executed by the wasm engine; it is also displayed on the
 * page ("under the hood"), so it is written to read well.
 *
 * The loop is a plain ReAct agent: block on `chidori.input()` for the next
 * user message, then alternate `chidori.prompt()` (decide) and
 * `chidori.tool()` (act) until the model replies. Every effect is journaled,
 * so the whole conversation suspends, resumes, and replays offline.
 *
 * Each feed event is one JSON line on the console — the page renders the
 * chat (bubbles, tool cards) purely from the journaled console output.
 *
 * This is only the *default* implementation: the chat can rewrite it. The
 * agent's tool set includes read_source / update_source / reset_source, and
 * an accepted edit is hot-swapped in at the end of the turn — the journal
 * replays against the new code (modify-and-resume).
 */
export const DEFAULT_AGENT_SOURCE = `type Decision = { tool?: string; args?: unknown; reply?: string };
type Message = { role: string; content: string };

const transcript: Message[] = [];

function emit(event: unknown): void {
  console.log(JSON.stringify(event));
}

async function turn(userText: string): Promise<void> {
  transcript.push({ role: 'user', content: userText });
  emit({ kind: 'user', text: userText });

  for (let hop = 0; hop < 6; hop++) {
    // The host answers with one JSON decision: {tool, args} or {reply}.
    const raw = await chidori.prompt(JSON.stringify(transcript), {
      protocol: 'chat-v1',
    });
    let decision: Decision;
    try {
      decision = JSON.parse(String(raw)) as Decision;
    } catch (err) {
      decision = { reply: String(raw) };
    }

    if (decision.tool) {
      let result: unknown;
      try {
        result = await chidori.tool(decision.tool, decision.args);
      } catch (err) {
        result = { error: String(err) };
      }
      emit({ kind: 'tool', name: decision.tool, args: decision.args, result });
      transcript.push({
        role: 'tool',
        content: JSON.stringify({ name: decision.tool, result }),
      });
      continue;
    }

    const reply = decision.reply ? String(decision.reply) : '\\u2026';
    emit({ kind: 'assistant', text: reply });
    transcript.push({ role: 'assistant', content: reply });
    return;
  }

  const bail = 'I hit the tool-call limit for this turn \\u2014 ask me to continue.';
  emit({ kind: 'assistant', text: bail });
  transcript.push({ role: 'assistant', content: bail });
}

async function main(): Promise<void> {
  for (;;) {
    const text = await chidori.input('message');
    await turn(String(text));
  }
}
main();
`;
