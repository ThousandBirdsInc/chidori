// Function-scoped recursion in both shapes the family tiers resolve:
// MUTUAL recursion through captured bindings (isEven/isOdd — the resolver
// pins whatever compatible closure each callee-position cell holds, so the
// pair becomes a two-member family: windowed on the interpreter, two
// mutually-calling native functions under the opt-in JIT) and SELF-recursion
// through a `const` binding (gcd). Deterministic (no RNG) so every runtime
// computes the same checksum.
(function () {
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
})();
