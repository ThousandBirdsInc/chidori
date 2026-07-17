// Supply-Chain Sentinel
// (No /// <reference types="@1kbirds/chidori/agent-env" /> here: the published
// 3.6.0 types are stale, so tsconfig `paths` maps chidori:agent to the in-repo
// SDK source instead. See review round 4, Finding 2.)
//
// Audits a Rust project's direct dependencies against the live ecosystem:
//   1. parses data/Cargo.toml + data/Cargo.lock with an npm package
//      (smol-toml) inside a chidori.step value checkpoint
//   2. gives the model two real tools — crates.io metadata and the OSV
//      vulnerability database — and has it triage each dependency
//      (one tool-loop prompt per dependency, three in flight at a time)
//   3. validates every verdict with zod before trusting it
//   4. asks a human to approve publication (chidori.input)
//   5. writes AUDIT.md to the workspace
//
// Scripted mode (deterministic, for recorded runs / CI):
//   chidori run agent.ts --trusted --model deepseek-v4-flash \
//     --input '{"decision": "publish"}'
// Interactive mode: omit `decision` and answer at the terminal.

import { chidori, run, defineTool } from "chidori:agent";
// Third TOML parser tried: smol-toml fails on cyclic ESM imports, confbox on
// the node:module builtin. fast-toml is a CommonJS leaf, which loads.
// Validation is valibot because zod (v3 AND v4) dies at import evaluation
// ("Cannot access binding before initialization" — internal ESM cycle).
// See review round 4, Findings 3 and 4.
import TOML from "fast-toml";
import * as v from "valibot";

const parse = (s: string) => TOML.parse(s);

type Dep = { name: string; req: string; resolved: string | null };

const Verdict = v.object({
  name: v.string(),
  risk: v.picklist(["low", "medium", "high"]),
  headline: v.string(),
  reasons: v.pipe(v.array(v.string()), v.minLength(1)),
  recommendation: v.string(),
  advisory_ids: v.array(v.string()),
});
type VerdictT = v.InferOutput<typeof Verdict>;

run(async (input: { top?: number; decision?: string }) => {
  const top = input.top ?? 10;

  // 1. Parse both manifests — pure compute on ~120KB of TOML, so wrap it in
  // a value checkpoint: resume and replay never re-pay the parse.
  const manifestRaw = await chidori.workspace.read("data/Cargo.toml");
  const lockRaw = await chidori.workspace.read("data/Cargo.lock");
  const deps: Dep[] = await chidori.step("parse-manifests", () => {
    const manifest = parse(manifestRaw) as {
      dependencies?: Record<string, string | { version?: string; path?: string }>;
    };
    const lock = parse(lockRaw) as {
      package?: { name: string; version: string }[];
    };
    const resolvedByName = new Map<string, string>();
    for (const p of lock.package ?? []) {
      // Workspace lockfiles can list several versions of one crate; for a
      // direct dependency the highest one is the one the manifest resolves to.
      const prev = resolvedByName.get(p.name);
      if (!prev || compareSemver(p.version, prev) > 0) resolvedByName.set(p.name, p.version);
    }
    const out: Dep[] = [];
    for (const [name, spec] of Object.entries(manifest.dependencies ?? {})) {
      if (typeof spec === "object" && spec.path) continue; // workspace-local crates
      const req = typeof spec === "string" ? spec : (spec.version ?? "*");
      out.push({ name, req, resolved: resolvedByName.get(name) ?? null });
    }
    return out;
  });
  const audited = deps.slice(0, top);
  await chidori.log("manifests parsed", { direct: deps.length, auditing: audited.length });

  // 2. Live-ecosystem tools, defined inline. Every fetch inside them is a
  // recorded host call: replay serves the same registry answers forever.
  const crateInfo = defineTool({
    name: "crate_info",
    description:
      "Look up a crate on crates.io: latest stable version, when it was last " +
      "updated, total downloads, repository URL, and whether the newest release " +
      "is yanked. Use this to judge staleness and how far behind the project is.",
    parameters: {
      type: "object",
      properties: { name: { type: "string", description: "Crate name, e.g. 'tokio'" } },
      required: ["name"],
    },
    run: async (args: { name: string }) => {
      const res = await fetch(`https://crates.io/api/v1/crates/${args.name}`, {
        headers: { "user-agent": "chidori-supply-chain-sentinel-demo" },
      });
      if (!res.ok) return { error: `crates.io returned ${res.status}` };
      const body = (await res.json()) as {
        crate: {
          max_stable_version: string;
          updated_at: string;
          downloads: number;
          repository: string | null;
          description: string | null;
        };
        versions?: { num: string; yanked: boolean; created_at: string }[];
      };
      const latest = (body.versions ?? []).find(
        (v) => v.num === body.crate.max_stable_version,
      );
      return {
        name: args.name,
        latest_stable: body.crate.max_stable_version,
        latest_release_date: latest?.created_at ?? null,
        latest_yanked: latest?.yanked ?? false,
        crate_updated_at: body.crate.updated_at,
        total_downloads: body.crate.downloads,
        repository: body.crate.repository,
        description: body.crate.description,
      };
    },
  });

  const advisories = defineTool({
    name: "advisories",
    description:
      "Query the OSV vulnerability database for known advisories against a " +
      "crate, optionally at a specific resolved version. Returns advisory ids " +
      "and summaries. An empty list means no known advisories.",
    parameters: {
      type: "object",
      properties: {
        name: { type: "string", description: "Crate name" },
        version: {
          type: "string",
          description: "Resolved version to check (omit to list all advisories)",
        },
      },
      required: ["name"],
    },
    run: async (args: { name: string; version?: string }) => {
      const query: Record<string, unknown> = {
        package: { name: args.name, ecosystem: "crates.io" },
      };
      if (args.version) query.version = args.version;
      const res = await fetch("https://api.osv.dev/v1/query", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(query),
      });
      if (!res.ok) return { error: `osv.dev returned ${res.status}` };
      const body = (await res.json()) as {
        vulns?: { id: string; summary?: string; aliases?: string[] }[];
      };
      return {
        name: args.name,
        version_checked: args.version ?? null,
        advisories: (body.vulns ?? []).slice(0, 10).map((v) => ({
          id: v.id,
          aliases: v.aliases ?? [],
          summary: v.summary ?? "(no summary)",
        })),
      };
    },
  });

  // 3. Triage each dependency with a tool loop; validate the model's JSON
  // with zod before letting it anywhere near the report.
  const verdicts: VerdictT[] = await chidori.util.parallel(
    audited.map((dep) => async () => {
      const raw = await chidori.prompt(
        `You are auditing the Rust dependency "${dep.name}" ` +
          `(manifest requirement "${dep.req}", resolved in Cargo.lock to ` +
          `${dep.resolved ?? "unknown"}).\n\n` +
          `Use crate_info to check staleness/yank status and advisories to check ` +
          `for known vulnerabilities at the resolved version. Then reply with ONLY ` +
          `a JSON object:\n` +
          `{"name": "...", "risk": "low"|"medium"|"high", "headline": "<one sentence>", ` +
          `"reasons": ["..."], "recommendation": "<what the maintainers should do>", ` +
          `"advisory_ids": ["..."]}\n\n` +
          `Risk rubric: high = a known advisory affects the resolved version, or the ` +
          `latest release is yanked; medium = advisories exist against other versions, ` +
          `or the crate looks unmaintained (no release in 2+ years) while the project ` +
          `pins an old major; low = current, maintained, no relevant advisories.`,
        {
          type: "subagent",
          tools: [crateInfo, advisories],
          maxTurns: 6,
          format: "json",
          maxTokens: 1600,
        },
      );
      return v.parse(Verdict, raw);
    }),
    { concurrency: 3 },
  );
  const counts = { high: 0, medium: 0, low: 0 };
  for (const v of verdicts) counts[v.risk]++;
  await chidori.log("triage complete", counts);

  // 4. One summary prompt over the validated verdicts.
  const summary = await chidori.prompt(
    `Write a short executive summary (<=150 words) of this dependency audit for ` +
      `the maintainers of the "chidori" Rust project. Be concrete: name the ` +
      `crates that need attention and why. Verdicts:\n` +
      JSON.stringify(verdicts, null, 2),
    // 500 was enough in most runs, but one crash-resumed continuation burned
    // the whole budget on hidden reasoning and returned "" — which this agent
    // then published. Budget generously for reasoning models.
    { type: "final", maxTokens: 1200 },
  );

  const report = renderReport(summary, verdicts, deps.length);

  // 5. Human gate. Scripted runs pass `decision`; interactive runs answer at
  // the terminal (or via a paused session under `chidori serve`).
  const decision =
    input.decision ??
    (await chidori.input(`Publish AUDIT.md? (${counts.high} high / ${counts.medium} medium / ${counts.low} low)`, {
      type: "approval",
      choices: ["publish", "discard"],
      default: "discard",
      details: report,
    }));
  if (decision !== "publish") {
    return { published: false, counts, summary };
  }

  // 6. Publish.
  await chidori.workspace.write("AUDIT.md", report, { language: "markdown" });
  return { published: true, counts, summary };
});

function renderReport(summary: string, verdicts: VerdictT[], directCount: number): string {
  const order = { high: 0, medium: 1, low: 2 } as const;
  const sorted = [...verdicts].sort((a, b) => order[a.risk] - order[b.risk]);
  const lines = [
    "# Dependency audit — chidori",
    "",
    `Audited ${verdicts.length} of ${directCount} direct dependencies against crates.io and OSV.`,
    "",
    "## Summary",
    "",
    summary.trim(),
    "",
    "## Verdicts",
    "",
  ];
  for (const v of sorted) {
    lines.push(`### ${v.name} — ${v.risk.toUpperCase()}`);
    lines.push("");
    lines.push(v.headline.trim());
    lines.push("");
    for (const r of v.reasons) lines.push(`- ${r}`);
    if (v.advisory_ids.length) lines.push(`- Advisories: ${v.advisory_ids.join(", ")}`);
    lines.push(`- **Recommendation:** ${v.recommendation}`);
    lines.push("");
  }
  return lines.join("\n");
}

function compareSemver(a: string, b: string): number {
  const pa = a.split(/[.+-]/).map((x) => parseInt(x, 10) || 0);
  const pb = b.split(/[.+-]/).map((x) => parseInt(x, 10) || 0);
  for (let i = 0; i < 3; i++) {
    if ((pa[i] ?? 0) !== (pb[i] ?? 0)) return (pa[i] ?? 0) - (pb[i] ?? 0);
  }
  return 0;
}
