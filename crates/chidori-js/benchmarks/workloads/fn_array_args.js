// Array-typed function-kernel arguments — the `(a, i) => a[i]` accessor
// class (docs/js-performance-roadmap §6.10): tiny helpers taking an array
// parameter, called hot from loops the caller keeps generic (the call site
// itself is what a function kernel accelerates). Covers element reads, a
// dense `.length` read, and a whole loop inside the kernelized body.
function get(a, i) {
  return a[i];
}
function dot(a, b, n) {
  let s = 0;
  for (let i = 0; i < n; i++) {
    s += a[i] * b[i];
  }
  return s;
}
function count(a) {
  return a.length;
}
const N = 1000;
const x = new Array(N);
const y = new Array(N);
for (let i = 0; i < N; i++) {
  x[i] = i * 0.5;
  y[i] = (i % 7) + 1;
}
let h = 0;
for (let r = 0; r < 2000; r++) {
  for (let i = 0; i < N; i++) {
    h = (h + get(x, i) * 2) % 1000003;
  }
  h = (h + dot(x, y, N) + count(x)) % 1000003;
}
console.log("RESULT=" + h);
