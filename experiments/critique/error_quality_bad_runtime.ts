// Experiment E7a: runtime error message quality.
// Throws three levels deep so we can judge whether the reported stack trace
// carries useful frames, file names, and line numbers.
import { chidori, run } from "chidori:agent";

function levelThree(): never {
  const obj: any = undefined;
  return obj.property.access; // TypeError here, line 8
}
function levelTwo() { return levelThree(); }
function levelOne() { return levelTwo(); }

run(async () => {
  await chidori.log("about to explode");
  return levelOne();
});
