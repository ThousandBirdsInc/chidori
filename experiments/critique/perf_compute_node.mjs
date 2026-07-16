// Node.js baseline for perf_compute.ts — identical workload, plain Node.
function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }

const t0 = Date.now();
const fibResult = fib(27);
const t1 = Date.now();

let acc = 0;
const arr = [];
for (let i = 0; i < 200_000; i++) arr.push({ i, s: "x" + (i % 100) });
for (const o of arr) acc += o.i % 7;
const t2 = Date.now();

let s = "";
for (let i = 0; i < 20_000; i++) s += i.toString(36);
const t3 = Date.now();

console.log(JSON.stringify({
  fibResult, acc, strLen: s.length,
  ms: { fib27: t1 - t0, churn200k: t2 - t1, strings20k: t3 - t2, total: t3 - t0 },
}));
