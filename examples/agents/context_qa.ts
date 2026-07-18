import { chidori, run } from "chidori:agent";

/**
 * Cache-aware multi-turn Q&A over a fixed corpus, using `chidori.context`.
 *
 * The system instructions and the corpus are identical for every question, so
 * they are laid out once as an immutable, cache-marked prefix. The provider
 * bills that prefix at the full rate exactly once (the cache write); every
 * later question reads it from the prompt cache at a steep discount. Compare
 * the `cache_creation`/`cache_read` token counts across the prompt records in
 * `chidori trace <run_id>`.
 *
 * Run:
 *   chidori run examples/agents/context_qa.ts \
 *     --input '{"corpus": "Section 1: All deploys require review. Section 2: Rollbacks are automatic.", "questions": ["Who approves deploys?", "What happens on a bad deploy?"]}'
 */
run(async (input: { corpus: string; questions: string[] }) => {
  // The stable head, built ONCE and frozen as a cacheable prefix.
  const base = chidori
    .context()
    .system(
      "You are a policy analyst. Answer ONLY from the provided corpus. " +
        "Cite section numbers. If the corpus is silent, say so.",
    )
    .doc("policy-corpus", input.corpus)
    .cacheBreakpoint("5m");

  const answers: { question: string; answer: string }[] = [];
  let ctx = base;
  for (const question of input.questions) {
    // Explicit window management: while the running Q&A tail stays under
    // ~8K estimated tokens this is a pure no-op; past it, the older turns
    // are folded into one recorded summary segment (the corpus head and the
    // newest two turns survive verbatim).
    ctx = await ctx.compact({ budgetTokens: 8000 });
    ctx = ctx.user(question);
    const { text, context } = await ctx.prompt({ type: "final" });
    ctx = context; // assistant turn appended; the corpus prefix stays shared
    answers.push({ question, answer: text });
    await chidori.log("answered", {
      question,
      contextDigest: ctx.digest().slice(0, 12),
    });
  }

  return { answers };
});
