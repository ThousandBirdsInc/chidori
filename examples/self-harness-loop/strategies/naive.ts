import type { Chidori } from "chidori:agent";

/**
 * The incumbent strategy, extracted as a branch variant: one search attempt,
 * no retry. This is the behavior that failed in production — kept in the
 * experiment so the comparison is honest (same anchored prefix, same tool,
 * only the strategy differs).
 */

type StrategyInput = { task: string };

export async function agent(input: StrategyInput, chidori: Chidori) {
  await chidori.log("naive: single attempt, no retry");
  const results = await chidori.tool("flaky_search", {
    query: input.task,
    attempt: 1,
  });
  return { strategy: "naive", results };
}
