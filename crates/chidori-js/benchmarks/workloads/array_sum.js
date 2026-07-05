// Dense-array traversal with index arithmetic — the `s += a[i]` class the
// typed loop kernels' element access targets: reads, in-place writes, a
// two-array dot product, and a nested 2D walk. Deterministic fill (no RNG)
// so every runtime computes the same checksum.
const N = 200_000;
const ROUNDS = 5;
const a = new Array(N);
const b = new Array(N);
for (let i = 0; i < N; i++) {
  a[i] = (i * 7919) % 10007;
  b[i] = (i * 104729) % 7919;
}
let checksum = 0;
for (let r = 0; r < ROUNDS; r++) {
  // read + accumulate
  let s = 0;
  for (let i = 0; i < a.length; i++) {
    s += a[i];
  }
  // dot product
  let d = 0;
  for (let i = 0; i < a.length; i++) {
    d += a[i] * b[i];
  }
  // in-place transform
  for (let i = 0; i < a.length; i++) {
    a[i] = (a[i] + b[i]) % 10007;
  }
  checksum = (checksum + s + d) % 9007199254740991;
}
// nested 2D walk
let m = 0;
for (let i = 0; i < 500; i++) {
  for (let j = 0; j < 500; j++) {
    m += (i * j) % 13;
  }
}
console.log("RESULT=" + (checksum + m));
