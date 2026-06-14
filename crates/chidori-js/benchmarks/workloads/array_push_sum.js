// Array growth + indexed read loop.
// Mirrors the `array_push_sum` criterion micro-benchmark, scaled up.
const N = 500_000;
const a = [];
for (let i = 0; i < N; i++) a.push(i);
let s = 0;
for (let i = 0; i < a.length; i++) s += a[i];
console.log("RESULT=" + s);
