// Object property get/set in a loop (shape lookups, map access).
// Mirrors the `property_access` criterion micro-benchmark, scaled up.
const N = 1_000_000;
const o = { a: 0, b: 0, c: 0 };
for (let i = 0; i < N; i++) {
  o.a = i;
  o.b = o.a + 1;
  o.c = o.b + o.a;
}
console.log("RESULT=" + o.c);
