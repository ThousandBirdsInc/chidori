import { chidori, run, type AgentJson, type JsonObject } from "chidori:agent";

/**
 * An autonomous "worker" agent: it loops — think, call a tool, observe the
 * result, repeat — until it produces an answer with no further tool calls.
 *
 * The loop is author-driven via `context.respond()`, which returns the model's
 * structured turn (`toolCalls` + `text`). Tool results are appended back to the
 * context with `toolResult(...)`, and the next `respond()` continues from there.
 * Every turn and tool call is a durable host call, so the whole run replays for
 * free.
 *
 * Run:
 *   chidori run examples/agents/worker.ts \
 *     --input task="Reverse the word 'chidori' and tell me the result." \
 *     --tools examples/tools
 */
run(async (input: { task: string; maxSteps?: number }) => {
  const maxSteps = input.maxSteps ?? 8;

  let ctx = chidori
    .context()
    .system(
      "You are an autonomous worker. Use the available tools to complete the " +
        "task. Call a tool when it helps; when you are finished, reply with a " +
        "final answer and no tool calls.",
    )
    .tools(["reverse"]) // tool names discovered from the --tools directory
    .user(input.task);

  const steps: { tool: string; input: JsonObject; result: AgentJson }[] = [];

  for (let step = 0; step < maxSteps; step++) {
    const { response, context } = await ctx.respond({ type: "final" });
    ctx = context; // the assistant turn (incl. tool-use blocks) is now in ctx

    // No tool calls means the worker is done.
    if (!response.toolCalls || response.toolCalls.length === 0) {
      return { answer: response.content, steps };
    }

    // Run each requested tool and feed the result back for the next turn.
    for (const call of response.toolCalls) {
      const result = await chidori.tool(call.name, call.input);
      steps.push({ tool: call.name, input: call.input, result });
      ctx = ctx.toolResult(call.id, JSON.stringify(result));
    }
  }

  return { answer: "(stopped: reached maxSteps without finishing)", steps };
});
