#!/usr/bin/env node
// Cross-runtime benchmark harness for the chidori-js JavaScript engine.
//
// Runs each workload in `workloads/` as a standalone script under chidori-js,
// Node.js, and Bun, measuring subprocess wall-clock time. The same `.js` file
// is fed to all three runtimes and every workload prints a single `RESULT=...`
// line, so the harness can confirm the runtimes agree before trusting the
// timings (a fast-but-wrong engine is not a faster engine).
//
// Each runtime pays a fixed process-startup cost (binary load + realm setup).
// We measure that separately with `workloads/startup.js` and subtract it to
// report an execution-only estimate alongside the raw total.
//
// Usage:
//   node benchmarks/run.mjs [options]
//
// Options:
//   --runs N            timed runs per workload (default 5)
//   --warmup N          untimed warmup runs per workload (default 1)
//   --filter SUBSTR     only run workloads whose name contains SUBSTR
//   --runtimes LIST     comma-separated subset of {chidori,node,bun}
//   --chidori-bin PATH  path to the chidori `run` example binary
//   --no-build          do not `cargo build` the chidori binary first
//   --json PATH         also write the full results as JSON to PATH
//   --quick             shorthand for --runs 3 --warmup 0
//   -h, --help          show this help
//
// Environment:
//   CHIDORI_RUN_BIN     same as --chidori-bin

import { execFile, execFileSync } from "node:child_process";
import { existsSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

const execFileP = promisify(execFile);
const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, "..", "..", "..");
const WORKLOAD_DIR = join(HERE, "workloads");

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

function parseArgs(argv) {
  const opts = {
    runs: 5,
    warmup: 1,
    filter: null,
    runtimes: null,
    chidoriBin: process.env.CHIDORI_RUN_BIN || null,
    build: true,
    json: null,
  };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const next = () => argv[++i];
    switch (a) {
      case "--runs": opts.runs = Number(next()); break;
      case "--warmup": opts.warmup = Number(next()); break;
      case "--filter": opts.filter = next(); break;
      case "--runtimes": opts.runtimes = next().split(",").map((s) => s.trim()).filter(Boolean); break;
      case "--chidori-bin": opts.chidoriBin = next(); break;
      case "--no-build": opts.build = false; break;
      case "--json": opts.json = next(); break;
      case "--quick": opts.runs = 3; opts.warmup = 0; break;
      case "-h": case "--help": printHelp(); process.exit(0); break;
      default:
        console.error(`unknown option: ${a}`);
        printHelp();
        process.exit(2);
    }
  }
  if (!Number.isFinite(opts.runs) || opts.runs < 1) {
    console.error("--runs must be a positive integer");
    process.exit(2);
  }
  return opts;
}

function printHelp() {
  // The banner comment at the top of this file is the canonical help text.
  console.log(
    [
      "Cross-runtime benchmark harness for chidori-js (vs Node.js and Bun).",
      "",
      "Usage: node benchmarks/run.mjs [options]",
      "",
      "  --runs N            timed runs per workload (default 5)",
      "  --warmup N          untimed warmup runs per workload (default 1)",
      "  --filter SUBSTR     only run workloads whose name contains SUBSTR",
      "  --runtimes LIST     comma-separated subset of {chidori,node,bun}",
      "  --chidori-bin PATH  path to the chidori `run` example binary",
      "  --no-build          do not cargo build the chidori binary first",
      "  --json PATH         also write the full results as JSON to PATH",
      "  --quick             shorthand for --runs 3 --warmup 0",
      "  -h, --help          show this help",
    ].join("\n"),
  );
}

// ---------------------------------------------------------------------------
// Runtime discovery
// ---------------------------------------------------------------------------

function whichSync(cmd) {
  try {
    return execFileSync("sh", ["-c", `command -v ${cmd}`], { encoding: "utf8" }).trim() || null;
  } catch {
    return null;
  }
}

// Build the chidori `run` example in release mode and return its path. The
// binary reads a JS file path as argv[1], evaluates it, and prints console
// output followed by `=> <value>` — we only care about the `RESULT=` line.
function resolveChidoriBin(opts) {
  if (opts.chidoriBin) {
    if (!existsSync(opts.chidoriBin)) {
      throw new Error(`chidori binary not found: ${opts.chidoriBin}`);
    }
    return opts.chidoriBin;
  }
  const builtPath = join(REPO_ROOT, "target", "release", "examples", "run");
  if (opts.build) {
    process.stderr.write("building chidori-js run example (release)... ");
    execFileSync("cargo", ["build", "--release", "-q", "-p", "chidori-js", "--example", "run"], {
      cwd: REPO_ROOT,
      stdio: ["ignore", "ignore", "inherit"],
    });
    process.stderr.write("done\n");
  }
  if (!existsSync(builtPath)) {
    throw new Error(
      `chidori binary not found at ${builtPath}. Build it with:\n` +
        `  cargo build --release -p chidori-js --example run\n` +
        `or pass --chidori-bin / --no-build with CHIDORI_RUN_BIN.`,
    );
  }
  return builtPath;
}

function discoverRuntimes(opts) {
  const names = opts.runtimes ?? ["chidori", "node", "bun"];
  // Resolve each requested runtime lazily so excluding chidori doesn't force a
  // Rust build, and a missing optional runtime (bun) is skipped, not fatal.
  const resolvers = {
    chidori: () => ({ name: "chidori", cmd: resolveChidoriBin(opts), args: [] }),
    node: () => ({ name: "node", cmd: process.execPath, args: [] }),
    bun: () => {
      const bun = whichSync("bun");
      return bun ? { name: "bun", cmd: bun, args: [] } : null;
    },
  };

  const selected = [];
  for (const n of names) {
    const make = resolvers[n];
    if (!make) {
      process.stderr.write(`warning: unknown runtime '${n}', skipping\n`);
      continue;
    }
    const rt = make();
    if (rt) selected.push(rt);
    else process.stderr.write(`warning: runtime '${n}' not available, skipping\n`);
  }
  if (selected.length === 0) throw new Error("no runtimes available to benchmark");
  return selected;
}

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

const RESULT_RE = /RESULT=(.*)/;

// Run one workload once under one runtime, returning { ms, result }.
async function runOnce(runtime, file) {
  const start = process.hrtime.bigint();
  let stdout;
  try {
    ({ stdout } = await execFileP(runtime.cmd, [...runtime.args, file], {
      maxBuffer: 64 * 1024 * 1024,
    }));
  } catch (err) {
    throw new Error(`${runtime.name} failed on ${file}: ${err.message}`);
  }
  const ms = Number(process.hrtime.bigint() - start) / 1e6;
  const m = stdout.match(RESULT_RE);
  if (!m) throw new Error(`${runtime.name} produced no RESULT= line for ${file}`);
  return { ms, result: m[1].trim() };
}

async function timeWorkload(runtime, file, runs, warmup) {
  for (let i = 0; i < warmup; i++) await runOnce(runtime, file);
  const samples = [];
  let result = null;
  for (let i = 0; i < runs; i++) {
    const r = await runOnce(runtime, file);
    samples.push(r.ms);
    result = r.result;
  }
  return { samples, result, ...summarize(samples) };
}

function summarize(samples) {
  const sorted = [...samples].sort((a, b) => a - b);
  const n = sorted.length;
  const median = n % 2 ? sorted[(n - 1) / 2] : (sorted[n / 2 - 1] + sorted[n / 2]) / 2;
  const mean = sorted.reduce((a, b) => a + b, 0) / n;
  return { min: sorted[0], max: sorted[n - 1], median, mean };
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

function fmtMs(ms) {
  if (ms == null) return "—";
  if (ms >= 1000) return (ms / 1000).toFixed(2) + "s";
  return ms.toFixed(1) + "ms";
}

function pad(s, w) {
  s = String(s);
  return s.length >= w ? s : s + " ".repeat(w - s.length);
}
function padLeft(s, w) {
  s = String(s);
  return s.length >= w ? s : " ".repeat(w - s.length) + s;
}

function printTable(workloads, runtimes, baselines) {
  const rtNames = runtimes.map((r) => r.name);
  // Fastest runtime per workload is the reference for the "x" speedup columns.
  const COL = 11;
  const nameW = Math.max(12, ...workloads.map((w) => w.name.length));

  console.log("\nExecution-only time (subprocess wall-clock minus startup baseline)");
  console.log("Startup baselines: " + rtNames.map((n) => `${n} ${fmtMs(baselines[n])}`).join("  "));
  console.log("");

  // Header.
  let header = pad("workload", nameW);
  for (const n of rtNames) header += "  " + padLeft(n, COL);
  header += "  " + padLeft("fastest", 9);
  console.log(header);
  console.log("-".repeat(header.length));

  for (const w of workloads) {
    let row = pad(w.name, nameW);
    const execByName = {};
    for (const n of rtNames) {
      const r = w.byRuntime[n];
      const exec = r ? Math.max(0, r.median - baselines[n]) : null;
      execByName[n] = exec;
    }
    const finite = Object.values(execByName).filter((v) => v != null && v > 0);
    const best = finite.length ? Math.min(...finite) : null;
    let fastestName = "";
    for (const n of rtNames) {
      const exec = execByName[n];
      if (exec == null) { row += "  " + padLeft("—", COL); continue; }
      const factor = best && exec > 0 ? exec / best : 1;
      const cell = factor <= 1.001 ? fmtMs(exec) : `${fmtMs(exec)} ${factor.toFixed(1)}x`;
      if (exec === best) fastestName = n;
      row += "  " + padLeft(cell, COL);
    }
    row += "  " + padLeft(fastestName, 9);
    console.log(row);
  }

  console.log("\nTotal time including startup (raw subprocess wall-clock, median)");
  let header2 = pad("workload", nameW);
  for (const n of rtNames) header2 += "  " + padLeft(n, COL);
  console.log(header2);
  console.log("-".repeat(header2.length));
  for (const w of workloads) {
    let row = pad(w.name, nameW);
    for (const n of rtNames) {
      const r = w.byRuntime[n];
      row += "  " + padLeft(r ? fmtMs(r.median) : "—", COL);
    }
    console.log(row);
  }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  const runtimes = discoverRuntimes(opts);

  // Discover workloads (excluding the startup baseline probe).
  const { readdirSync } = await import("node:fs");
  let files = readdirSync(WORKLOAD_DIR)
    .filter((f) => f.endsWith(".js") && f !== "startup.js")
    .sort();
  if (opts.filter) files = files.filter((f) => f.includes(opts.filter));
  if (files.length === 0) throw new Error("no workloads matched");

  console.log(
    `chidori-js cross-runtime benchmarks\n` +
      `runtimes: ${runtimes.map((r) => r.name).join(", ")}  |  ` +
      `runs: ${opts.runs}  warmup: ${opts.warmup}  workloads: ${files.length}`,
  );

  // Startup baseline per runtime (median of a few extra runs — it's cheap).
  const baselines = {};
  const startupFile = join(WORKLOAD_DIR, "startup.js");
  for (const rt of runtimes) {
    const { median } = await timeWorkload(rt, startupFile, Math.max(opts.runs, 5), 1);
    baselines[rt.name] = median;
  }

  const workloads = [];
  let mismatches = 0;
  for (const f of files) {
    const file = join(WORKLOAD_DIR, f);
    const name = f.replace(/\.js$/, "");
    process.stderr.write(`  ${name} ... `);
    const byRuntime = {};
    const results = new Set();
    for (const rt of runtimes) {
      const r = await timeWorkload(rt, file, opts.runs, opts.warmup);
      byRuntime[rt.name] = r;
      results.add(r.result);
    }
    const agree = results.size === 1;
    if (!agree) {
      mismatches++;
      process.stderr.write("RESULT MISMATCH\n");
      for (const rt of runtimes) {
        process.stderr.write(`      ${rt.name}: ${byRuntime[rt.name].result}\n`);
      }
    } else {
      process.stderr.write("ok\n");
    }
    workloads.push({ name, byRuntime, agree, result: [...results][0] });
  }

  printTable(workloads, runtimes, baselines);

  if (mismatches > 0) {
    console.log(
      `\n⚠️  ${mismatches} workload(s) disagreed across runtimes — timings above are not comparable for those rows.`,
    );
  }

  if (opts.json) {
    const payload = {
      generatedAt: new Date().toISOString(),
      options: { runs: opts.runs, warmup: opts.warmup },
      runtimes: runtimes.map((r) => ({ name: r.name, cmd: r.cmd })),
      baselinesMs: baselines,
      workloads: workloads.map((w) => ({
        name: w.name,
        agree: w.agree,
        result: w.result,
        runtimes: Object.fromEntries(
          Object.entries(w.byRuntime).map(([n, r]) => [
            n,
            { median: r.median, min: r.min, max: r.max, mean: r.mean, samples: r.samples },
          ]),
        ),
      })),
    };
    writeFileSync(opts.json, JSON.stringify(payload, null, 2));
    console.log(`\nwrote ${opts.json}`);
  }

  process.exit(mismatches > 0 ? 1 : 0);
}

main().catch((err) => {
  console.error("\nerror: " + err.message);
  process.exit(1);
});
