// Shared between the agent and the tools: parse the `git log --numstat` dump
// in data/gitlog.txt into structured commit records.

export type Commit = {
  hash: string;
  date: string;
  subject: string;
  body: string;
  files: { path: string; added: number; deleted: number }[];
}

export function parseGitLog(raw: string): Commit[] {
  const commits: Commit[] = [];
  let cur: Commit | null = null;
  let inBody = false;

  for (const line of raw.split("\n")) {
    if (line.startsWith("COMMIT ")) {
      if (cur) commits.push(cur);
      cur = { hash: line.slice(7).trim(), date: "", subject: "", body: "", files: [] };
      inBody = false;
    } else if (!cur) {
      continue;
    } else if (line.startsWith("DATE ")) {
      cur.date = line.slice(5).trim();
    } else if (line.startsWith("SUBJECT ")) {
      cur.subject = line.slice(8).trim();
    } else if (line === "BODY") {
      inBody = true;
    } else if (line === "FILES") {
      inBody = false;
      cur.body = cur.body.trim();
    } else if (inBody) {
      cur.body += line + "\n";
    } else {
      // numstat line: "<added>\t<deleted>\t<path>"
      const m = line.match(/^(\d+|-)\t(\d+|-)\t(.+)$/);
      if (m) {
        cur.files.push({
          path: m[3],
          added: m[1] === "-" ? 0 : parseInt(m[1], 10),
          deleted: m[2] === "-" ? 0 : parseInt(m[2], 10),
        });
      }
    }
  }
  if (cur) commits.push(cur);
  return commits;
}

export function commitSummaryLine(c: Commit): string {
  const touched = c.files.length;
  const churn = c.files.reduce((n, f) => n + f.added + f.deleted, 0);
  return `${c.hash} (${c.date}) ${c.subject} [${touched} files, ${churn} lines]`;
}
