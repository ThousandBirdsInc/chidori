// Higher-order array methods (map/filter/reduce + per-element closures).
// Mirrors the `array_hof` criterion micro-benchmark, scaled up.
const N = 200_000;
const a = [];
for (let i = 0; i < N; i++) a.push(i);
const result = a
  .map((x) => x * x)
  .filter((x) => x % 2 === 0)
  .reduce((p, c) => p + c, 0);
console.log("RESULT=" + result);
