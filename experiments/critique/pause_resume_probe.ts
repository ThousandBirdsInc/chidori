// Experiment E9: pause-for-human durability.
// Does some work, then suspends on chidori.input(). The process should exit
// with the run parked on disk. Resuming later (a different process) must
// replay steps 1-2 from the journal without re-executing the tool, feed the
// human answer in, and finish. The `beforePause` value existing unchanged in
// the final output proves state survived the process boundary.
import { chidori, run } from "chidori:agent";

run(async () => {
  const work = await chidori.tool("side_effect", { label: "pre-pause" });
  const beforePause = `computed-at:${Date.now()}`;
  const answer = await chidori.input("Approve shipping the release? (yes/no)");
  return { work, beforePause, humanAnswer: answer, decision: answer === "yes" ? "shipped" : "held" };
});
