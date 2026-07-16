// Experiment E8: raw interpreter throughput.
// Pure-compute workload (no host calls inside the hot loop) so we measure the
// chidori-js interpreter itself: recursive fib, array/object churn, and
// string building. Compare wall time against Node.js running perf_compute_node.mjs.
import { chidori, run } from "chidori:agent";

function fib(n: number): number {
  return n < 2 ? n : fib(n - 1) + fib(n - 2);
}

run(async () => {
  const t0 = Date.now();
  const fibResult = fib(27);
  const t1 = Date.now();

  // Object/array churn: build and reduce 200k small objects.
  let acc = 0;
  const arr: { i: number; s: string }[] = [];
  for (let i = 0; i < 200_000; i++) arr.push({ i, s: "x" + (i % 100) });
  for (const o of arr) acc += o.i % 7;
  const t2 = Date.now();

  // String building.
  let s = "";
  for (let i = 0; i < 20_000; i++) s += i.toString(36);
  const t3 = Date.now();

  return {
    fibResult,
    acc,
    strLen: s.length,
    ms: { fib27: t1 - t0, churn200k: t2 - t1, strings20k: t3 - t2, total: t3 - t0 },
  };
});
