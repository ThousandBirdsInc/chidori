// A tool with an observable side effect: every real invocation appends a line
// to an on-disk ledger inside the workspace. Used by exactly_once_probe.ts to
// verify that replay does NOT re-execute the tool (the ledger length is the
// ground truth for how many times the effect actually ran).
import { chidori } from "chidori:agent";

export default async function side_effect(input: { label: string }) {
  const ledgerPath = "ledger.txt";
  let existing = "";
  try {
    existing = await chidori.workspace.read(ledgerPath);
  } catch {
    existing = "";
  }
  const line = `invoked:${input.label}\n`;
  await chidori.workspace.write(ledgerPath, existing + line);
  const count = (existing + line).split("\n").filter(Boolean).length;
  return { invocations: count };
}
