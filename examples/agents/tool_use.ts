import { chidori, run, defineTool } from "chidori:agent";

// A tool is just a function with a documented signature: `defineTool` pairs a
// `run` function with the `name`/`description`/`parameters` the model calls it
// by — no `tools/` directory, no registration, no `--tools` flag. `run`
// executes in the agent's own VM, so you can call it directly (here, without an
// LLM) for zero setup, or hand it to the model with `prompt("...", { tools:
// [reverse] })`.
const reverse = defineTool({
  name: "reverse",
  description: "Reverse a string and return it. A sample tool — replace with your own.",
  parameters: {
    type: "object",
    properties: {
      text: { type: "string", description: "The text to reverse" },
    },
    required: ["text"],
  },
  run: async (args: { text: string }) => ({
    reversed: [...String(args.text)].reverse().join(""),
  }),
});

run(async (input: { query: string }) => {
  // Invoke the tool body directly — no LLM, no network. In an LLM agent you'd
  // instead pass `reverse` in `prompt(text, { tools: [reverse] })` and let the
  // model decide when to call it; the runtime runs the loop for you.
  const result = await reverse.run({ text: input.query }, chidori);
  return { result };
});
