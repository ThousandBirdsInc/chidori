// Typed-array element traffic — the numeric-buffer class (DSP, embeddings,
// image/audio scratch): fill, sum, dot product, and in-place transform over
// Float64Array plus an Int32Array bit-mix pass. Mirrors array_sum.js but on
// typed arrays, which the typed loop kernels do NOT yet accept as a base
// (element access only translates for dense `Internal::Array`), so this runs
// on the generic interpreter path today. Deterministic fill (no RNG) so every
// runtime computes the same checksum.
const N = 50_000;
const ROUNDS = 4;
const a = new Float64Array(N);
const b = new Float64Array(N);
for (let i = 0; i < N; i++) {
  a[i] = (i * 7919) % 10007;
  b[i] = (i * 104729) % 7919;
}
let checksum = 0;
for (let r = 0; r < ROUNDS; r++) {
  let s = 0;
  for (let i = 0; i < a.length; i++) {
    s += a[i];
  }
  let d = 0;
  for (let i = 0; i < a.length; i++) {
    d += a[i] * b[i];
  }
  for (let i = 0; i < a.length; i++) {
    a[i] = (a[i] + b[i]) % 10007;
  }
  checksum = (checksum + s + d) % 9007199254740991;
}
// Int32Array pass: integer wrap + shift semantics.
const m = new Int32Array(1024);
for (let i = 0; i < m.length; i++) {
  m[i] = (i * 2654435761) | 0;
}
let mix = 0;
for (let r = 0; r < 50; r++) {
  for (let i = 0; i < m.length; i++) {
    mix = (mix ^ m[i]) + ((mix << 5) | 0) | 0;
  }
}
console.log("RESULT=" + (checksum + (mix >>> 0)));
