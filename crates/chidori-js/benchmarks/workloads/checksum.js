// Byte-buffer checksum — Adler-32 over a Uint8Array, the parsing/hashing/
// codec class of workload: byte loads, small-integer adds, modulo by a
// constant, and a bit-pack at the end of each round. The whole inner loop is
// integer-typed byte traffic, so it exercises the narrow typed-array element
// path end to end. Deterministic fill (no RNG) so every runtime computes the
// same checksum.
(function () {
  const N = 500_000;
  const ROUNDS = 10;
  const MOD = 65521;
  const data = new Uint8Array(N);
  for (let i = 0; i < N; i++) {
    data[i] = (i * 131 + 7) & 0xff;
  }
  let result = 0;
  for (let r = 0; r < ROUNDS; r++) {
    let a = 1;
    let b = 0;
    for (let i = 0; i < data.length; i++) {
      a = (a + data[i]) % MOD;
      b = (b + a) % MOD;
    }
    // (b << 16) | a wraps int32 for b >= 32768; >>> 0 reads the same bits
    // back as the unsigned Adler value.
    const adler = ((b << 16) | a) >>> 0;
    result = (result + adler) % 9007199254740991;
    // Perturb one byte so rounds are not identical.
    data[r] = (data[r] + 1) & 0xff;
  }
  console.log("RESULT=" + result);
})();
