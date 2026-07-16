// Experiment E5: determinism policy probe.
// Chidori claims replay is byte-identical because nondeterminism (clock,
// randomness) is captured by runtime policy. This agent deliberately uses
// every ambient nondeterminism source we can reach and returns them, so a
// record run and a replay run can be diffed.
import { chidori, run } from "chidori:agent";

run(async () => {
  const now = Date.now();
  const wallClock = new Date().toISOString();
  const rand = [Math.random(), Math.random(), Math.random()];
  const uuidish = Math.random().toString(36).slice(2);

  // A timer is a host effect; its firing should be journaled.
  await new Promise((resolve) => setTimeout(resolve, 50));
  const afterTimer = Date.now();

  return { now, wallClock, rand, uuidish, afterTimer, elapsed: afterTimer - now };
});
