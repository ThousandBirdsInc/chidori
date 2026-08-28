// The recursion shape the fast tiers still decline: MUTUAL recursion through
// function-scoped bindings (isEven/isOdd — the recursion-family executor and
// the opt-in JIT resolve mutual partners through GLOBAL bindings only), so
// that pair runs on the generic call path and this row measures the call
// ceremony extending the family resolution would remove. gcd, by contrast,
// is SELF-recursion through a `const` binding, which the tiers (and the JIT,
// as a native recursive function) do take. Deterministic (no RNG) so every
// runtime computes the same checksum.
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
