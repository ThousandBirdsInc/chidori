// Experiment E4: exactly-once verification.
// Calls a tool that appends to an on-disk ledger (tools/side_effect.ts).
// Record run: ledger should gain exactly 3 lines. Replay run: the tool result
// must come from the journal, so the ledger must NOT grow. The returned
// counts let us diff record vs replay output for byte-identity too.
import { chidori, run } from "chidori:agent";

run(async () => {
  const a = await chidori.tool("side_effect", { label: "first" });
  const b = await chidori.tool("side_effect", { label: "second" });
  const c = await chidori.tool("side_effect", { label: "third" });
  return { counts: [a, b, c] };
});
