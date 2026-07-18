import type { Chidori } from "chidori:agent";

/**
 * Step 1 of the self-harness loop: the worker in production.
 *
 * It answers a task by searching the knowledge base — with the NAIVE strategy:
 * one attempt, no retry. The bundled `flaky_search` tool times out on first
 * attempts, so this run fails with `tool_error`. That failure — streamed to
 * tael as an error span, persisted as a replayable checkpoint — is the raw
 * material the rest of the loop mines.
 *
 * Run (with tael listening on :4317):
 *   OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 \
 *   chidori run examples/self-harness-loop/worker.ts \
 *     --input task="deployment rollback procedure"
 */
export async function agent(input: { task: string }, chidori: Chidori) {
  await chidori.log(`worker: researching "${input.task}"`);

  // The naive strategy: one shot, no retry. This is the weakness.
  const results = await chidori.tool("flaky_search", {
    query: input.task,
    attempt: 1,
  });

  const summary = await chidori.prompt(
    `Summarize these search results for the task "${input.task}":\n` +
      JSON.stringify(results),
  );
  return { task: input.task, summary };
}
