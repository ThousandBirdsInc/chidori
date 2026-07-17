import type { Chidori, ToolDefinition } from "chidori:agent";
import { parseGitLog } from "./parse.ts";

export const tool: ToolDefinition = {
  name: "search_commits",
  description:
    "Case-insensitive keyword search over commit subjects, bodies, and changed " +
    "file paths in the release window. Returns matching hashes and subjects.",
  parameters: {
    type: "object",
    properties: {
      query: { type: "string", description: "Keyword or phrase to search for" },
    },
    required: ["query"],
  },
};

export async function run(args: { query: string }, chidori: Chidori) {
  const raw = await chidori.workspace.read("data/gitlog.txt");
  const commits = parseGitLog(raw);
  const q = args.query.toLowerCase();
  const hits = commits.filter(
    (c) =>
      c.subject.toLowerCase().includes(q) ||
      c.body.toLowerCase().includes(q) ||
      c.files.some((f) => f.path.toLowerCase().includes(q)),
  );
  return {
    query: args.query,
    matches: hits.slice(0, 12).map((c) => ({ hash: c.hash, date: c.date, subject: c.subject })),
    total: hits.length,
  };
}
