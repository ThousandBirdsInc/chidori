import { chidori } from "chidori";

export const tool = {
  name: "review_batch",
  description: "Review a batch of items for one pipeline stage, logging each step.",
  parameters: {
    type: "object",
    properties: {
      stage: { type: "number" },
      items: { type: "number" },
    },
    required: ["stage", "items"],
  },
};

/**
 * Runs inside the tool's own VM but shares the runtime context, so every host
 * call it makes here is recorded as a CHILD of this tool's call — i.e. these
 * `chidori.log` spans nest UNDER the `tool.call review_batch` span in the trace.
 */
export async function run(args: { stage: number; items: number }) {
  const flagged: number[] = [];
  for (let item = 1; item <= args.items; item++) {
    await chidori.log(`review_batch: scanned item ${item}/${args.items}`, {
      stage: args.stage,
      item,
    });
    // Flag every third item as needing attention (an extra nested step).
    if (item % 3 === 0) {
      flagged.push(item);
      await chidori.log(`review_batch: flagged item ${item}`, {
        stage: args.stage,
        item,
      });
    }
  }
  return { stage: args.stage, scanned: args.items, flagged };
}
