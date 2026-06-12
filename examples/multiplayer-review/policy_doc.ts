import type { Chidori, Signal } from "chidori";

/**
 * Multiplayer policy-doc drafting agent (docs/signals.md §7).
 *
 * Three participants collaborate on ONE live run:
 *   - a human editor (e.g. Mara) reviews drafts and asks for changes,
 *   - a compliance-checker agent pushes an approve/changes verdict,
 *   - a human lead (e.g. Sam) re-scopes the doc mid-run.
 *
 * None of these are agent-initiated `input()` questions. The reviewers PUSH a
 * `review` signal when they have it; the lead STEERS with a `steer` signal
 * whenever he wants. The agent only consumes those pushes at the points it
 * declares safe:
 *   - `chidori.signal("review")` BLOCKS — nothing to do until a review lands;
 *     the run idles cheaply on disk and resumes when a review is delivered (or
 *     was already queued in the durable mailbox).
 *   - `chidori.pollSignal("steer")` is NON-BLOCKING — steering is optional; the
 *     agent checks the mailbox and moves on if it is empty.
 *
 * Every signal is recorded in the call log, so the whole multiplayer session
 * replays deterministically (`chidori resume <run-id>`): the reviews, the
 * verdict, and the steering are replayed from their recorded CallRecords with
 * no human re-contacted and no compliance agent re-run.
 *
 * Deliver signals with:
 *   POST /sessions/{id}/signal  { name, payload, from }
 * (see the README for full curl examples).
 *
 * `writeDraft`/`revise` are kept as local helpers so the example is runnable
 * offline with no LLM provider; swap them for `chidori.prompt(...)` calls to
 * get real model spans in the trace.
 */

type Brief = { topic?: string; audience?: string; priority?: string; scope?: string };
type Review = { decision: "approve" | "changes"; notes: string };
type Steer = { priority?: string; scope?: string };

export async function agent(input: Brief, chidori: Chidori) {
  let brief: Brief = {
    topic: input.topic ?? "data-retention policy",
    audience: input.audience ?? "all staff",
  };

  let draft = await writeDraft(chidori, brief); // "expensive": stands in for LLM + retrieval
  let round = 0;

  while (true) {
    round++;
    await chidori.log(`draft round ${round} ready`, { words: draft.length });

    // Open this run to reviewers. The compliance agent AND the human editor both
    // send a "review" signal; whichever lands first (or is already queued in the
    // mailbox) is consumed here. `from` tells us who reviewed.
    const review: Signal<Review> = await chidori.signal<Review>("review");
    await chidori.log("review received", {
      from: review.from,
      decision: review.payload.decision,
    });

    if (review.payload.decision === "approve") {
      return {
        status: "published",
        rounds: round,
        approvedBy: review.from,
        draft,
      };
    }

    // A reviewer asked for changes — revise and loop. Before revising,
    // opportunistically pick up any steering the lead pushed (non-blocking;
    // null if none waiting).
    const steer = await chidori.pollSignal<Steer>("steer");
    if (steer) {
      await chidori.log("scope changed mid-run", { from: steer.from, ...steer.payload });
      brief = { ...brief, ...steer.payload }; // re-scope without restarting
    }

    draft = await revise(chidori, draft, review.payload.notes, brief);
  }
}

/** Stand-in for the expensive drafting step (LLM + retrieval). Local + offline. */
async function writeDraft(chidori: Chidori, brief: Brief): Promise<string> {
  await chidori.log("writing draft", { topic: brief.topic, scope: brief.scope });
  return `# ${brief.topic} (for ${brief.audience})\n\nInitial draft body.`;
}

/** Stand-in for the revision step. Appends the reviewer's notes to the draft. */
async function revise(
  chidori: Chidori,
  draft: string,
  notes: string,
  brief: Brief,
): Promise<string> {
  await chidori.log("revising draft", { notes, scope: brief.scope });
  return `${draft}\n\n## Revision\nAddressed: ${notes}`;
}
