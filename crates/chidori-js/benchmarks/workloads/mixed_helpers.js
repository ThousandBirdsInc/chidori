// Mixed "glue code": small helper functions over objects and strings — the
// shape of real agent/tool plumbing that the typed kernel tiers deliberately
// do NOT cover (non-numeric bodies, string building, property traffic across
// calls, for-in). This is the register-bytecode tier's home turf: with loop
// kernels declining every function here, the interpreter itself carries the
// whole workload.
const N = 60_000;

function normalize(rec) {
  const out = { id: rec.id | 0, label: "", score: 0 };
  for (const k in rec) {
    if (k === "id") continue;
    const v = rec[k];
    if (typeof v === "number") out.score = out.score + v;
    else if (typeof v === "string") out.label = out.label ? out.label + ":" + v : v;
  }
  return out;
}

function render(rec) {
  return "[" + rec.id + "] " + (rec.label || "?") + " => " + rec.score;
}

function classify(score) {
  return score > 40 ? "hi" : score > 15 ? "mid" : "lo";
}

const buckets = { hi: 0, mid: 0, lo: 0 };
let text = "";
let checksum = 0;
for (let i = 0; i < N; i++) {
  const rec = { id: i, name: "item" + (i % 7), a: i % 13, b: (i * 3) % 29, kind: i % 2 ? "x" : "y" };
  const norm = normalize(rec);
  const line = render(norm);
  buckets[classify(norm.score)]++;
  checksum = (checksum + line.length + norm.score) % 1000000007;
  if ((i & 1023) === 0) text += line + "\n";
}
console.log("RESULT=" + checksum + "|" + buckets.hi + "," + buckets.mid + "," + buckets.lo + "|" + text.length);
