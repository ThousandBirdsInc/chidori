export async function agent(input, chidori) {
  // A trivial "draft" so the run reaches a named listen point without needing
  // an LLM. The interesting part is the signal pause.
  const draft = `draft for ${input.topic ?? "untitled"}`;
  await chidori.log("draft ready", { topic: input.topic ?? "untitled" });

  // Pause until a `review` signal is delivered (or one is already queued).
  const review = await chidori.signal("review");

  return {
    topic: input.topic ?? "untitled",
    draft,
    decision: review.payload?.decision ?? null,
    reviewedBy: review.from,
  };
}
