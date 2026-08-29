// String building (concatenation + number→string coercion).
// Mirrors the `string_build` criterion micro-benchmark, scaled up.
// Kept modest because naive `+=` string building is O(n²) on an engine without
// rope/cord string representation; chidori-js builds the string eagerly.
(function () {
  const N = 30_000;
  let s = "";
  for (let i = 0; i < N; i++) s += "x" + i;
  console.log("RESULT=" + s.length);
})();
