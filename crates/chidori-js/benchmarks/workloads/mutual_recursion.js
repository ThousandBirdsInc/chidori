// Recursion shapes the function kernels decline today, so the whole workload
// runs on the generic call path: mutual recursion between two globals
// (isEven/isOdd — kernels guard exactly ONE self global), boolean returns
// (recursive kernels must return numbers), and self-recursion through a
// `const` binding rather than a global name (gcd). This measures the call
// ceremony those extensions would remove. Deterministic (no RNG) so every
// runtime computes the same checksum.
function isEven(n) {
  return n === 0 ? true : isOdd(n - 1);
}
function isOdd(n) {
  return n === 0 ? false : isEven(n - 1);
}
const gcd = (a, b) => (b === 0 ? a : gcd(b, a % b));

const N = 20_000;
let checksum = 0;
for (let i = 0; i < N; i++) {
  if (isEven(i % 300)) checksum++;
  checksum += gcd(i + 123456, 991);
}
console.log("RESULT=" + checksum);
