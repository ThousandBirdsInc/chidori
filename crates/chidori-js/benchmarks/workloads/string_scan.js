// Per-code-unit string reads — the tokenizer/parser/hash idiom:
// `for (i = 0; i < s.length; i++) s.charCodeAt(i)`. This is the class of
// access the charCodeAt/charAt/codePointAt builtins serve. chidori-js today
// materializes the FULL string as a fresh Vec<u16> on every call
// (`builtins/string.rs::units_this` → `to_utf16_vec`), making this loop
// O(n²) with n heap allocations; this workload exists to measure that and
// to gate the fix. Deterministic content (no RNG) so every runtime computes
// the same checksum.
const N = 8192; // string length in code units
const ROUNDS = 12;
let s = "";
for (let i = 0; i < N; i++) {
  s += String.fromCharCode(97 + ((i * 31) % 26));
}
let h = 0;
for (let r = 0; r < ROUNDS; r++) {
  for (let i = 0; i < s.length; i++) {
    h = (h * 31 + s.charCodeAt(i)) % 1000000007;
  }
}
// A smaller charAt pass so the sibling accessor is covered too.
let vowels = 0;
for (let i = 0; i < s.length; i++) {
  const c = s.charAt(i);
  if (c === "a" || c === "e" || c === "i" || c === "o" || c === "u") vowels++;
}
console.log("RESULT=" + (h + vowels));
