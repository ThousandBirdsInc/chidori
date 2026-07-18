import type { BranchOutcome, Chidori } from "chidori:agent";

/**
 * Step 4 of the self-harness loop: the controlled experiment.
 *
 * The run does its shared prefix work once, then forks with `chidori.branch`
 * into the incumbent strategy (naive) and the reflector's proposal
 * (retry_with_backoff) — same anchored state, same tool, one variable. Each
 * variant's spans are stamped with `chidori.branch_label`, so
 * `tael experiment compare <run-id>` scores the A/B with no extra
 * instrumentation. The winning variant's checkpoint becomes the regression
 * fixture.
 *
 * Run:
 *   OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 \
 *   chidori run examples/self-harness-loop/experiment.ts \
 *     --input task="deployment rollback procedure"
 */
export async function agent(input: { task: string }, chidori: Chidori) {
  const task = input.task ?? "deployment rollback procedure";

  // Shared prefix: whatever context-building the worker would do before the
  // risky call. Paid once; every branch inherits it as its anchor.
  await chidori.log(`experiment: anchoring shared prefix for "${task}"`);

  const outcomes = await chidori.branch([
    {
      label: "naive",
      source: "examples/self-harness-loop/strategies/naive.ts",
      input: { task },
    },
    {
      label: "retry_with_backoff",
      source: "examples/self-harness-loop/strategies/retry_with_backoff.ts",
      input: { task },
    },
  ]);

  for (const o of outcomes) {
    await chidori.log(
      `branch ${o.label}: ${o.status}` +
        (o.status === "failed" ? ` (${o.error})` : ""),
    );
  }

  const winners = outcomes.filter((o) => o.status === "completed");
  const winner = winners.length > 0 ? winners[0].label : null;
  return {
    task,
    winner,
    outcomes: outcomes.map((o: BranchOutcome) => ({
      label: o.label,
      status: o.status,
      ...(o.status === "failed" ? { error: o.error } : {}),
    })),
  };
}
