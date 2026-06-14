// Recursion + function-call overhead (frame setup/teardown).
// Mirrors the `fib_recursive` criterion micro-benchmark, scaled up.
function fib(n) {
  return n < 2 ? n : fib(n - 1) + fib(n - 2);
}
console.log("RESULT=" + fib(30));
