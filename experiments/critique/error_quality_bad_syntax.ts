// Experiment E7b: parse error message quality (missing closing brace).
import { chidori, run } from "chidori:agent";

run(async () => {
  const value = { a: 1, b: 2 ;
  return value;
});
