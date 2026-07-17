/// <reference types="@1kbirds/chidori/agent-env" />
// Release-Notes Concierge
//
// Given a dump of a repo's git history (data/gitlog.txt), it:
//   1. parses it into structured commits (chidori.step — memoized pure compute)
//   2. clusters the window into release themes (one structured-output prompt)
//   3. investigates each theme with the built-in provider tool loop
//      (commit_detail / search_commits tools)
//   4. drafts the notes in an editorial conversation() that remembers the
//      house style learned in previous sessions (chidori.memory)
//   5. loops on human feedback (chidori.input) until approved
//   6. publishes RELEASE_NOTES.md to the workspace
//
// Scripted mode (deterministic, for recorded runs / tests):
//   chidori run agent.ts --trusted --tools tools \
//     --input '{"feedback": ["Tighten the intro.", "approve"]}'
// Interactive mode: omit `feedback` and answer at the terminal.

import { chidori, run } from "chidori:agent";
import { parseGitLog, commitSummaryLine } from "./tools/parse.ts";

type Theme = {
  title: string;
  rationale: string;
  commit_hashes: string[];
}

run(async (input: { audience?: string; feedback?: string[]; maxThemes?: number }) => {
  const audience = input.audience ?? "developers evaluating the project";
  const maxThemes = input.maxThemes ?? 3;

  // 1. Load and parse the release window — parsing is pure compute, so wrap
  // it in a value checkpoint: replays and resumes never re-pay it.
  const raw = await chidori.workspace.read("data/gitlog.txt");
  const commits = await chidori.step("parse-gitlog", () => parseGitLog(raw));
  await chidori.log("window parsed", { commits: commits.length });

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
    // format:"json" silently falls back to the raw string on truncation/parse
    // failure — without this guard the agent "succeeds" with an empty product.
    throw new Error("theme clustering returned no themes: " + String(clustering).slice(0, 200));
  }
  await chidori.log("themes", { titles: themes.map((t) => t.title) });

  // 3. Investigate each theme. The built-in tool loop lets the model pull
  // commit bodies and file lists on demand instead of us stuffing 90KB of
  // history into every prompt.
  const sections: string[] = [];
  for (const theme of themes.slice(0, maxThemes)) {
    const section = await chidori.prompt(
      `You are researching the release theme "${theme.title}" ` +
        `(${theme.rationale}). Candidate commits: ${theme.commit_hashes.join(", ")}.\n` +
        `Use commit_detail to read what the key commits actually changed (bodies and ` +
        `file lists), and search_commits if you suspect related work outside the list. ` +
        `Then write the release-notes section: a "## ${theme.title}" heading, 2-4 ` +
        `crisp bullets grounded in what the commits really did, each citing hashes.`,
      { type: "draft", tools: ["commit_detail", "search_commits"], maxTurns: 8, maxTokens: 12000 },
    );
    sections.push(section);
    await chidori.log("section drafted", { theme: theme.title });
  }

  // 4. Editorial pass as a conversation, seeded with the house style this
  // desk has learned from previous sessions.
  // (as any): the published 3.6.0 types declare `memory(action, ...)`, but the
  // runtime and docs expose the `memory.get/set/...` namespace.
  const style = await (chidori.memory as any).get("house-style");
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
          details: draft, // in docs, missing from published InputOptions types
        } as any);
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
    await (chidori.memory as any).set("house-style", learned.trim());
  }

  await chidori.workspace.write("RELEASE_NOTES.md", draft);
  return {
    commits: commits.length,
    themes: themes.map((t) => t.title),
    editorialTurns: chat.length,
    published: "RELEASE_NOTES.md",
  };
});
