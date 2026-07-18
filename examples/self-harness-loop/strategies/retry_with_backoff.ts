import type { Chidori } from "chidori:agent";

/**
 * The reflector's proposed strategy: retry the flaky tool with backoff
 * instead of failing on the first transient error.
 */

type StrategyInput = { task: string };

export async function agent(input: StrategyInput, chidori: Chidori) {
  const maxAttempts = 3;
  for (let attempt = 1; attempt <= maxAttempts; attempt++) {
    try {
      const results = await chidori.tool("flaky_search", {
        query: input.task,
        attempt,
      });
      await chidori.log(`retry_with_backoff: succeeded on attempt ${attempt}`);
      return { strategy: "retry_with_backoff", attempts: attempt, results };
    } catch (err) {
      await chidori.log(
        `retry_with_backoff: attempt ${attempt} failed (${String(err)})`,
      );
      if (attempt === maxAttempts) throw err;
    }
  }
  throw new Error("unreachable");
}
