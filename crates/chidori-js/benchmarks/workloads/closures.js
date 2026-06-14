// Closures + higher-order calls in a loop (upvalue capture/read).
// Mirrors the `closures` criterion micro-benchmark, scaled up.
const N = 1_000_000;
function adder(n) {
  return function (x) {
    return x + n;
  };
}
const f = adder(5);
let s = 0;
for (let i = 0; i < N; i++) s = f(s) - 4;
console.log("RESULT=" + s);
