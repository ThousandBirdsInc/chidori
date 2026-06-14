// Tight numeric loop — interpreter dispatch + integer/float arithmetic.
// Mirrors the `arith_loop` criterion micro-benchmark, scaled up so execution
// dominates process startup when run as a standalone script.
const N = 1_000_000;
let s = 0;
for (let i = 0; i < N; i++) {
  s += i * 2 - (i % 3);
}
console.log("RESULT=" + s);
