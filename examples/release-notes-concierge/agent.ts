/// <reference types="@1kbirds/chidori/agent-env" />
// Release-Notes Concierge
//
// Given a dump of a repo's git history (data/gitlog.txt), it:
//   1. parses it into structured commits (chidori.step — memoized pure compute)
//   2. clusters the window into release themes (one structured-output prompt)
//   3. investigates each theme with a tool loop over two IMPORT-DEFINED tools
//      (defineTool — plain objects in this file, no tools/ directory; their
//      bodies run in the agent's own VM, so closures over the parsed commits
//      work and replay comes for free)
//   4. drafts the notes in an editorial conversation() that remembers the
//      house style learned in previous sessions (chidori.memory)
//   5. loops on human feedback (chidori.input) until approved
//   6. publishes RELEASE_NOTES.md to the workspace
//
// Scripted mode (deterministic, for recorded runs / tests):
//   chidori run agent.ts --trusted \
//     --input '{"feedback": ["Tighten the intro.", "approve"]}'
// Interactive mode: omit `feedback` and answer at the terminal.

import { chidori, run, defineTool } from "chidori:agent";
import { parseGitLog, commitSummaryLine, type Commit } from "./lib/parse.ts";

type Theme = {
  title: string;
  rationale: string;
  commit_hashes: string[];
};

run(async (input: { audience?: string; feedback?: string[]; maxThemes?: number }) => {
  const audience = input.audience ?? "developers evaluating the project";
  const maxThemes = input.maxThemes ?? 3;

  // 1. Load and parse the release window — parsing is pure compute, so wrap
  // it in a value checkpoint: replays and resumes never re-pay it.
  const raw = await chidori.workspace.read("data/gitlog.txt");
  const commits: Commit[] = await chidori.step("parse-gitlog", () => parseGitLog(raw));
  await chidori.log("window parsed", { commits: commits.length });

  // Tools as plain code: defined here, closing over the parsed commits —
  // no re-reading the dump per call, no separate tool files, no --tools flag.
  const commitDetail = defineTool({
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
    run: async (args: { hash: string }) => {
      const c = commits.find((x) => x.hash.startsWith(args.hash));
      if (!c) return { error: `no commit matching '${args.hash}' in the window` };
      return {
        hash: c.hash,
        date: c.date,
        subject: c.subject,
        body: c.body || "(no body)",
        files: c.files.map((f) => `${f.path} (+${f.added}/-${f.deleted})`),
      };
    },
  });

  const searchCommits = defineTool({
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
    run: async (args: { query: string }) => {
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
    },
  });

  const index = commits.map(commitSummaryLine).join("\n");

  // 2. Cluster the window into themes with structured output.
  const clustering = await chidori.prompt(
    `Here are the ${commits.length} commits in this release window, newest first:\n\n` +
      index +
      `\n\nGroup the genuinely user-facing changes into at most ${maxThemes} release ` +
      `themes for ${audience}. Ignore pure chores (CI, formatting). Reply as JSON: ` +
      `{"themes": [{"title": string, "rationale": string, "commit_hashes": [string]}]}`,
    // Reasoning models (deepseek-v4-flash) spend maxTokens on hidden reasoning
    // FIRST — 4000 produced zero visible output. Budget generously.
    { type: "plan", format: "json", maxTokens: 16000 },
  );
  const themes: Theme[] = (clustering as any).themes ?? [];
  if (themes.length === 0) {
    // format:"json" throws on truncated/unparseable output by default, so
    // this guard covers only the residual case: valid JSON of the wrong shape.
    throw new Error("theme clustering returned no themes: " + String(clustering).slice(0, 200));
  }
  await chidori.log("themes", { titles: themes.map((t) => t.title) });

  // 3. Investigate each theme. The tool loop lets the model pull commit
  // bodies and file lists on demand instead of us stuffing 90KB of history
  // into every prompt.
  const sections: string[] = [];
  for (const theme of themes.slice(0, maxThemes)) {
    const section = await chidori.prompt(
      `You are researching the release theme "${theme.title}" ` +
        `(${theme.rationale}). Candidate commits: ${theme.commit_hashes.join(", ")}.\n` +
        `Use commit_detail to read what the key commits actually changed (bodies and ` +
        `file lists), and search_commits if you suspect related work outside the list. ` +
        `Then write the release-notes section: a "## ${theme.title}" heading, 2-4 ` +
        `crisp bullets grounded in what the commits really did, each citing hashes.`,
      { type: "draft", tools: [commitDetail, searchCommits], maxTurns: 8, maxTokens: 12000 },
    );
    sections.push(section as string);
    await chidori.log("section drafted", { theme: theme.title });
  }

  // 4. Editorial pass as a conversation, seeded with the house style this
  // desk has learned from previous sessions.
  const style = await chidori.memory.get("house-style");
  const chat = chidori.conversation({
    system:
      "You are the release-notes editor. Assemble and revise release notes in " +
      "clean Markdown: a one-paragraph intro, then the provided sections. Be " +
      "accurate; never invent changes. Output only the document itself." +
      (style ? `\n\nHouse style learned from previous releases:\n${style}` : ""),
    maxTokens: 12000,
  });

  let draft = await chat.say(
    `Assemble v1 of the release notes for ${audience} from these sections:\n\n` +
      sections.join("\n\n"),
  );

  // 5. Human feedback loop: scripted turns when provided (deterministic runs),
  // otherwise interactive input() pauses.
  const scripted = input.feedback;
  for (let round = 0; ; round++) {
    const fb = scripted
      ? (scripted[round] ?? "approve")
      : await chidori.input(`Feedback on draft v${round + 1}? (or 'approve')`, {
          details: draft,
        });
    if (String(fb).trim().toLowerCase() === "approve") break;
    if (round >= 4) break; // editorial patience budget
    draft = await chat.say(`Revise the full document per this feedback: ${fb}`);
  }

  // 6. Distill what this session taught us about the house style, so the next
  // run starts from it (chidori.memory persists across runs).
  const learned = await chat.say(
    "In at most 3 short imperative lines, state the durable style preferences " +
      "expressed in this session's feedback (or 'none').",
  );
  if (!/^\s*none\s*$/i.test(learned)) {
    await chidori.memory.set("house-style", learned.trim());
  }

  await chidori.workspace.write("RELEASE_NOTES.md", draft);
  return {
    commits: commits.length,
    themes: themes.map((t) => t.title),
    editorialTurns: chat.length,
    published: "RELEASE_NOTES.md",
  };
});
