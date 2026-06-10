import { chidori, run } from "chidori";

type Input = {
  /** Name shown in logs and as the agent name in the OTEL trace. */
  pipeline?: string;
  /** Number of review stages; each ends with a human checkpoint. */
  stages?: number;
  /** Items each stage "reviews" — one logged step (one span) per item. */
  itemsPerStage?: number;
};

/**
 * A long-running, human-in-the-loop pipeline (a toy "incident triage" run).
 *
 * Each stage reviews a batch of items — every item is a recorded `chidori.log`
 * host call, i.e. one span in the trace — and then PAUSES at a checkpoint for
 * the operator to type a decision. Under `chidori run`, `chidori.input` prints
 * the prompt and blocks on stdin (the terminal REPL), so the run lasts as long
 * as the interactive session. Type `continue`, `rerun`, `stop`, or any note.
 *
 * If `OTEL_EXPORTER_OTLP_ENDPOINT` points at tael (default 127.0.0.1:4317),
 * each host call streams out as a span *while the run is still going* — so you
 * watch the trace fill in across the session instead of all at once at the end.
 *
 * It's also durable: the call log is checkpointed under `.chidori/runs/<id>/`,
 * so `chidori resume <agent> <id>` replays your earlier answers from the
 * journal (no re-prompting) and continues — without re-emitting prior spans.
 */
run(async (input: Input) => {
  const pipeline = input.pipeline ?? "triage";
  const stages = clampInt(input.stages, 5, 1, 50);
  const itemsPerStage = clampInt(input.itemsPerStage, 4, 1, 50);

  await chidori.log(`pipeline '${pipeline}' starting`, { stages, itemsPerStage });

  const journal: Array<{ stage: number; reviewed: number; decision: string }> = [];
  let totalReviewed = 0;

  for (let stage = 1; stage <= stages; stage++) {
    await chidori.log(`stage ${stage}/${stages}: begin`, { stage });

    // Delegate the batch to a tool. The tool logs each item INTERNALLY, so those
    // spans nest under this `tool.call review_batch` span — that's the nesting
    // (parent_seq) in the trace, one level below the top-level agent calls.
    const result = await chidori.tool<
      { stage: number; items: number },
      { stage: number; scanned: number; flagged: number[] }
    >("review_batch", { stage, items: itemsPerStage });
    const reviewed = result.scanned;
    totalReviewed += reviewed;
    if (result.flagged.length > 0) {
      await chidori.log(`stage ${stage}: ${result.flagged.length} item(s) flagged`, {
        stage,
        flagged: result.flagged,
      });
    }

    // Checkpoint: pause for the human operator. With `chidori run` this reads a
    // line from your terminal; on the server it pauses for an HTTP resume.
    const answer = await chidori.input(
      `Stage ${stage}/${stages} reviewed ${reviewed} items ` +
        `(${totalReviewed} total). ` +
        `Type 'continue', 'rerun', 'stop', or a free-text note:`,
      { type: "checkpoint", choices: ["continue", "rerun", "stop"] },
    );
    const decision = answer.trim().toLowerCase() || "continue";
    await chidori.log(`stage ${stage}: operator said '${decision}'`, { stage, decision });
    journal.push({ stage, reviewed, decision });

    if (decision === "stop") {
      await chidori.log(`pipeline '${pipeline}' stopped by operator at stage ${stage}`, {
        stage,
      });
      return { pipeline, status: "stopped", stoppedAt: stage, totalReviewed, journal };
    }
    if (decision === "rerun") {
      // Re-do this stage: undo its tally and step back so the loop revisits it.
      totalReviewed -= reviewed;
      stage--;
    }
    // "continue" or any note → advance to the next stage.
  }

  await chidori.log(`pipeline '${pipeline}' complete`, { stages, totalReviewed });
  return { pipeline, status: "completed", totalReviewed, journal };
});

function clampInt(value: unknown, fallback: number, min: number, max: number): number {
  const n = typeof value === "number" ? value : parseInt(String(value ?? ""), 10);
  if (!Number.isFinite(n)) return fallback;
  return Math.max(min, Math.min(max, Math.trunc(n)));
}
