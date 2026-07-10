// Captured loop bounds and accumulators — bindings a nested closure
// captures stay heap CELLS after localization (docs/js-performance-roadmap
// §6.10), and this is the shape that exercises the kernel cell slots: the
// bound and the running total are both cells, read and written on every
// iteration, observed by the closures after. Deterministic content so every
// runtime computes the same checksum.
const N = 2000000;
let total = 0;
const peek = () => total; // captures `total` -> cell
const bound = () => N; // captures `N` -> cell
for (let i = 0; i < N; i++) {
  total += (i % 7) - (i & 3);
}
for (let i = 0; i < N; i++) {
  total += i % 5;
}
console.log("RESULT=" + (total + peek() + bound()));
