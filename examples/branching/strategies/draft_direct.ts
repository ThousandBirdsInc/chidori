import type { Chidori } from "chidori:agent";

/**
 * Strategy B: write a flowing draft straight from the research, no outline.
 * The variant under test against `outline_first.ts` — same anchored input,
 * different approach.
 */

type BranchInput = { topic: string; research: string };

export async function agent(input: BranchInput, chidori: Chidori) {
  await chidori.log("draft-direct: writing straight through");
  const draft = `# ${input.topic}\n\nIn short: ${input.research}. Told as one continuous narrative.`;
  return { strategy: "draft-direct", draft };
}
