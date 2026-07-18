import {
  chidori,
  run,
  defineTool,
  type AgentJson,
  type JsonObject,
  type ToolHandle,
} from "chidori:agent";

/**
 * An autonomous "worker" agent: it loops — think, call a tool, observe the
 * result, repeat — until it produces an answer with no further tool calls.
 *
 * The loop is author-driven via `context.respond()`, which returns the model's
 * structured turn (`toolCalls` + `text`). Tool results are appended back to the
 * context with `toolResult(...)`, and the next `respond()` continues from there.
 * (For the common case, `prompt(text, { tools, maxTurns })` runs this whole
 * loop for you — reach for the manual form only when you need per-step control.)
 *
 * A tool is just a function with a documented signature — a `name`, a
 * `description`, and JSON-schema `parameters`. `defineTool` staples that
 * signature onto the function, giving a plain handle you define inline or
 * import; its `run` executes in the agent's own VM. No `tools/` directory, no
 * `--tools` flag. Every model turn is a durable host call, so the run replays
 * for free.
 *
 * Run:
 *   chidori run examples/agents/worker.ts \
 *     --input task="Reverse the word 'chidori' and tell me the result."
 */

const reverse = defineTool({
  name: "reverse",
  description: "Reverse a string and return it.",
  parameters: {
    type: "object",
    properties: { text: { type: "string", description: "The text to reverse" } },
    required: ["text"],
  },
  run: async (args: { text: string }) => ({
    reversed: [...String(args.text)].reverse().join(""),
  }),
});

// The tools this worker can call, indexed by name so the loop can dispatch a
// model tool-call to the right handle. Typed as `ToolHandle` (JsonObject args)
// so any registered tool accepts the model's JSON tool-call input.
const toolbox = new Map<string, ToolHandle>([[reverse.name, reverse]]);

run(async (input: { task: string; maxSteps?: number }) => {
  const maxSteps = input.maxSteps ?? 8;

  let ctx = chidori
    .context()
    .system(
      "You are an autonomous worker. Use the available tools to complete the " +
        "task. Call a tool when it helps; when you are finished, reply with a " +
        "final answer and no tool calls.",
    )
    .tools([...toolbox.values()]) // pass the defineTool handles
    .user(input.task);

  const steps: { tool: string; input: JsonObject; result: AgentJson }[] = [];

  for (let step = 0; step < maxSteps; step++) {
    const { response, context } = await ctx.respond({ type: "final" });
    ctx = context; // the assistant turn (incl. tool-use blocks) is now in ctx

    // No tool calls means the worker is done.
    if (!response.toolCalls || response.toolCalls.length === 0) {
      return { answer: response.content, steps };
    }

    // Run each requested tool in-VM and feed the result back for the next turn.
    for (const call of response.toolCalls) {
      const tool = toolbox.get(call.name);
      const result = (
        tool ? await tool.run(call.input, chidori) : { error: `unknown tool: ${call.name}` }
      ) as AgentJson;
      steps.push({ tool: call.name, input: call.input, result });
      ctx = ctx.toolResult(call.id, JSON.stringify(result));
    }
  }

  return { answer: "(stopped: reached maxSteps without finishing)", steps };
});
