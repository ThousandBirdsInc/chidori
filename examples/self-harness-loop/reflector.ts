import type { Chidori } from "chidori:agent";

/**
 * Step 3 of the self-harness loop: the reflector — an agent whose job is to
 * improve another agent's harness.
 *
 * It reads the failed run's trajectory from tael's REST API (the same trace a
 * human would look at), asks the model to diagnose the failure and propose a
 * bounded harness edit, and writes the revised strategy into the workspace as
 * a branch-ready module. The proposal here is deliberately simple — a retry
 * strategy from a known-good template. Harness *proposal* quality is the
 * frontier model's job; this demo supplies the loop, not the mind.
 *
 * Run (after step 2 produced a failing trace):
 *   chidori run examples/self-harness-loop/reflector.ts \
 *     --input trace_id=<trace-id> --input tael_url=http://localhost:7701
 */

type ReflectorInput = { trace_id: string; tael_url?: string };

export async function agent(input: ReflectorInput, chidori: Chidori) {
  const tael = input.tael_url ?? "http://localhost:7701";

  // 1. Pull the failed trajectory — spans, statuses, error messages — from
  //    tael. This is a durable, recorded host call like any other.
  const response = await fetch(
    `${tael}/api/v1/traces/${input.trace_id}`,
  );
  const trace = await response.json();
  const spans: any[] = trace.spans ?? [];

  const failed = spans.filter((s) => s.status === "error");
  const errorSummary = failed
    .map(
      (s) =>
        `${s.operation}: ${s.attributes?.["exception.message"] ?? "unknown error"}`,
    )
    .join("\n");
  await chidori.log(
    `reflector: trace ${input.trace_id} has ${failed.length} error span(s)`,
  );

  // 2. Ask the model to diagnose and propose. (Under
  //    CHIDORI_TEST_LLM_RESPONSE this returns the canned response — the loop
  //    still exercises end to end without an API key.)
  const diagnosis = await chidori.prompt(
    `An agent run failed. Error spans:\n${errorSummary}\n\n` +
      `The failing tool call was flaky_search with a single attempt and no ` +
      `retry. Diagnose the root cause in one sentence and state the bounded ` +
      `harness change you would make.`,
  );
  await chidori.log(`reflector diagnosis: ${String(diagnosis).slice(0, 200)}`);

  // 3. Write the proposed strategy into the workspace as a branch-ready
  //    module. The write is durable, policy-gated, and recorded.
  const entry = await chidori.workspace.write(
    "strategies/retry_with_backoff.ts",
    RETRY_STRATEGY_SOURCE,
    { language: "typescript" },
  );

  return {
    trace_id: input.trace_id,
    error_spans: failed.length,
    diagnosis,
    proposed_strategy: entry.path,
    next: "chidori run examples/self-harness-loop/experiment.ts --input task=...",
  };
}

/**
 * The proposed harness edit: retry with backoff. Written (not imported) so the
 * experiment's branch variant is a real file the run anchors to — editable,
 * diffable, and independently re-runnable via `chidori branch-rerun`.
 */
const RETRY_STRATEGY_SOURCE = `import type { Chidori } from "chidori:agent";

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
      await chidori.log(\`retry_with_backoff: succeeded on attempt \${attempt}\`);
      return { strategy: "retry_with_backoff", attempts: attempt, results };
    } catch (err) {
      await chidori.log(
        \`retry_with_backoff: attempt \${attempt} failed (\${String(err)})\`,
      );
      if (attempt === maxAttempts) throw err;
    }
  }
  throw new Error("unreachable");
}
`;
