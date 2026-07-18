/// <reference types="@1kbirds/chidori/agent-env" />
import { chidori, run } from "chidori:agent";

/**
 * Standup Scribe — a weekly digest agent that lives with a team for weeks.
 *
 * Given a week of raw standup transcripts (workspace files under
 * `data/<week>/`), it reads them one day at a time in a single running
 * conversation, then:
 *
 *   - digests each day (template-driven prompt, `chidori.template`);
 *   - compacts the conversation when it outgrows a token budget
 *     (`Context.compact` — explicit, recorded, replayable);
 *   - updates a thread ledger that CARRIES OVER between weeks
 *     (`chidori.memory` — the ledger from week 1 is the starting context
 *     for week 2);
 *   - pauses for a human to approve the brief (`chidori.input`), then
 *     publishes it to the workspace (`briefs/<week>.md`).
 *
 * Run week by week:
 *   chidori run agent.ts --input week=week1 --model deepseek-v4-flash --trusted
 *   chidori run agent.ts --input week=week2 --model deepseek-v4-flash --trusted
 *
 * Then ask questions about the team interactively:
 *   chidori chat ask.ts --model deepseek-v4-flash
 */

type Thread = {
  id: string;
  title: string;
  owner: string;
  status: "open" | "blocked" | "resolved";
  note: string;
};

const TEAM = "Kestrel";

/** Tolerate a markdown fence around a JSON reply. */
function parseLedger(raw: string): Thread[] {
  const text = raw
    .trim()
    .replace(/^```(?:json)?\s*/i, "")
    .replace(/```\s*$/, "");
  const parsed = JSON.parse(text);
  if (!Array.isArray(parsed)) throw new Error("ledger reply was not a JSON array");
  return parsed as Thread[];
}

run(async (input: { week: string; budgetTokens?: number }) => {
  const week = input.week ?? "week1";
  const budgetTokens = input.budgetTokens ?? 2800;

  // -- Which days are we digesting? --------------------------------------
  const entries = await chidori.workspace.list();
  const days = entries
    .map((e) => e.path)
    .filter((p) => p.startsWith(`data/${week}/`) && p.endsWith(".md"))
    .sort();
  if (days.length === 0) throw new Error(`no transcripts under data/${week}/`);

  // -- Carry last week's threads in as context ---------------------------
  const prior = ((await chidori.memory.get("threads")) ?? []) as Thread[];
  const carried = prior.filter((t) => t.status !== "resolved");
  await chidori.log("starting week", {
    week,
    days: days.length,
    carriedThreads: carried.length,
  });

  const system = await chidori.template("prompts/system.jinja", { team: TEAM });
  let ctx = chidori.context().system(system);
  if (carried.length > 0) {
    ctx = ctx.doc(
      "thread-ledger",
      "Open threads carried over from last week:\n" +
        JSON.stringify(carried, null, 2),
    );
  }
  ctx = ctx.cacheBreakpoint("5m");

  // -- One running conversation over the week's standups -----------------
  const dailyDigests: { day: string; digest: string }[] = [];
  for (const path of days) {
    const before = ctx.estimateTokens();
    ctx = await ctx.compact({ budgetTokens, keepTurns: 2 });
    const compacted = ctx.estimateTokens() < before;

    const transcript = await chidori.workspace.read(path);
    const dayLabel = path.replace(`data/${week}/`, "").replace(".md", "");
    const prompt = await chidori.template("prompts/daily.jinja", {
      day_label: dayLabel,
      transcript,
    });
    ctx = ctx.user(prompt);
    const { text, context } = await ctx.prompt({
      type: "progress",
      maxTokens: 1200,
    });
    ctx = context;
    dailyDigests.push({ day: dayLabel, digest: text });
    await chidori.log("digested", {
      day: dayLabel,
      compacted,
      estTokens: ctx.estimateTokens(),
    });
  }

  // -- Update the thread ledger (structured output) ----------------------
  const threadsPrompt = await chidori.template("prompts/threads.jinja", {
    prior_count: carried.length,
  });
  const { text: ledgerRaw, context: afterLedger } = await ctx
    .user(threadsPrompt)
    .prompt({ type: "progress", maxTokens: 2000 });
  ctx = afterLedger;
  const ledger = parseLedger(ledgerRaw);
  await chidori.memory.set("threads", ledger);
  await chidori.memory.set("lastWeek", week);

  // -- Weekly brief, approved by a human, published to the workspace -----
  const weeklyPrompt = await chidori.template("prompts/weekly.jinja", {
    team: TEAM,
    week,
  });
  const { text: brief } = await ctx
    .user(weeklyPrompt)
    .prompt({ type: "final", maxTokens: 2500 });

  const verdict = await chidori.input(`Publish the ${week} brief?`, {
    type: "approval",
    choices: ["yes", "no"],
    default: "yes",
    details: brief,
  });
  let published: string | null = null;
  if (verdict.toLowerCase() !== "no") {
    const entry = await chidori.workspace.write(`briefs/${week}.md`, brief, {
      language: "markdown",
    });
    published = entry.path;
  }

  return {
    week,
    days: dailyDigests.map((d) => d.day),
    threads: ledger,
    published,
  };
});
