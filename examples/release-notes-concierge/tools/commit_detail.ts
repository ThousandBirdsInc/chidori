import type { Chidori, ToolDefinition } from "chidori:agent";
import { parseGitLog } from "./parse";

export const tool: ToolDefinition = {
  name: "commit_detail",
  description:
    "Look up the full details of one commit by its short hash: date, subject, " +
    "full body, and the list of files it changed with line counts. Use this to " +
    "understand what a commit actually did before describing it.",
  parameters: {
    type: "object",
    properties: {
      hash: { type: "string", description: "Short commit hash, e.g. 'eb3c788'" },
    },
    required: ["hash"],
  },
};

export async function run(args: { hash: string }, chidori: Chidori) {
  const raw = await chidori.workspace.read("data/gitlog.txt");
  const commits = parseGitLog(raw);
  const c = commits.find((x) => x.hash.startsWith(args.hash));
  if (!c) return { error: `no commit matching '${args.hash}' in the window` };
  return {
    hash: c.hash,
    date: c.date,
    subject: c.subject,
    body: c.body || "(no body)",
    files: c.files.map((f) => `${f.path} (+${f.added}/-${f.deleted})`),
  };
}
