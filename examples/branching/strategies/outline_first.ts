import type { Chidori } from "chidori";

/**
 * Strategy A: build an outline from the handed-over research, then expand each
 * point. Edit this file freely — re-running the parent re-anchors the branch
 * to the same shared prefix, so only this strategy's behavior changes.
 */

type BranchInput = { topic: string; research: string };

export async function agent(input: BranchInput, chidori: Chidori) {
  await chidori.log("outline-first: structuring before writing");
  const points = input.research.split(";").map((p) => p.trim());
  const outline = points.map((p, i) => `${i + 1}. ${p}`).join("\n");
  const draft = `# ${input.topic}\n\n${outline}\n\nEach point expanded in order, building on the last.`;
  return { strategy: "outline-first", draft };
}
