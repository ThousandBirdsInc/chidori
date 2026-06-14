// Array sorting with a comparator — comparator call overhead + the engine's
// sort implementation. Uses a deterministic LCG so every runtime sorts the
// same input and must agree on the checksum.
const N = 50_000;
const ROUNDS = 6;
let seed = 123456789;
function rnd() {
  // 32-bit LCG (glibc constants), kept in unsigned range.
  seed = (seed * 1103515245 + 12345) >>> 0;
  return seed;
}
let checksum = 0;
for (let r = 0; r < ROUNDS; r++) {
  const a = new Array(N);
  for (let i = 0; i < N; i++) a[i] = rnd();
  a.sort((x, y) => x - y);
  checksum = (checksum + a[0] + a[N - 1] + a[N >> 1]) >>> 0;
}
console.log("RESULT=" + checksum);
