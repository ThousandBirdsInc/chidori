#!/usr/bin/env node
// Cross-runtime benchmark harness for the chidori-js JavaScript engine.
//
// Runs each workload in `workloads/` as a standalone script under chidori-js,
// Node.js, and Bun, measuring subprocess wall-clock time and peak memory
// (max RSS). The same `.js` file is fed to all three runtimes and every
// workload prints a single `RESULT=...` line, so the harness can confirm the
// runtimes agree before trusting the numbers (a fast-but-wrong engine is not
// a faster engine).
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
//   --markdown PATH     also write a Markdown report to PATH (used by CI)
//   --mem-runs N        memory-measured runs per workload (default 3)
//   --no-memory         skip the peak-memory (max RSS) measurement
//   --quick             shorthand for --runs 3 --warmup 0 --mem-runs 1
//   -h, --help          show this help
//
// Environment:
//   CHIDORI_RUN_BIN     same as --chidori-bin

import { execFile, execFileSync, spawnSync } from "node:child_process";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
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
    markdown: null,
    memory: true,
    memRuns: 3,
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
      case "--markdown": opts.markdown = next(); break;
      case "--mem-runs": opts.memRuns = Number(next()); break;
      case "--no-memory": opts.memory = false; break;
      case "--quick": opts.runs = 3; opts.warmup = 0; opts.memRuns = 1; break;
      case "-h": case "--help": printHelp(); process.exit(0); break;
      default:
        console.error(`unknown option: ${a}`);
        printHelp();
        process.exit(2);
    }
  }
  if (!Number.isFinite(opts.runs) || opts.runs < 1 || !Number.isFinite(opts.memRuns) || opts.memRuns < 1) {
    console.error("--runs and --mem-runs must be positive integers");
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
      "  --markdown PATH     also write a Markdown report to PATH (used by CI)",
      "  --mem-runs N        memory-measured runs per workload (default 3)",
      "  --no-memory         skip the peak-memory (max RSS) measurement",
      "  --quick             shorthand for --runs 3 --warmup 0 --mem-runs 1",
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
    process.stderr.write("building chidori-js run example (release, mimalloc)... ");
    // `--features mimalloc` swaps the global allocator in the benchmark binary
    // only; the library build stays dependency-identical (see Cargo.toml).
    execFileSync(
      "cargo",
      ["build", "--release", "-q", "-p", "chidori-js", "--features", "mimalloc", "--example", "run"],
      {
        cwd: REPO_ROOT,
        stdio: ["ignore", "ignore", "inherit"],
      },
    );
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
// Peak-memory (max RSS) measurement
// ---------------------------------------------------------------------------
//
// Peak RSS of each subprocess is measured in dedicated extra runs (never the
// timed ones, so the timing methodology is untouched). Best available
// strategy wins:
//
//   1. GNU time (`/usr/bin/time -v`, or `gtime -v` from Homebrew) — exact
//      ru_maxrss from wait4(), reported in kbytes.
//   2. BSD time (`/usr/bin/time -l`, macOS) — exact, reported in bytes.
//   3. Linux /proc poller — sample the kernel-maintained `VmHWM` high-water
//      mark from `/proc/<pid>/status` every ~1ms while the child runs. Exact
//      whenever a sample lands after the peak (VmHWM is monotonic); for very
//      short-lived processes it can under-read slightly or miss entirely
//      (reported as —).
//
// If none applies (non-Linux without a usable `time`), the memory tables are
// skipped with a warning.

function detectMemoryStrategy() {
  const gnuParse = (s) => {
    const m = s.match(/Maximum resident set size \(kbytes\): (\d+)/);
    return m ? Number(m[1]) * 1024 : null;
  };
  const bsdParse = (s) => {
    const m = s.match(/(\d+)\s+maximum resident set size/);
    return m ? Number(m[1]) : null;
  };
  const candidates = [
    { cmd: "/usr/bin/time", flag: "-v", parse: gnuParse, label: "GNU time" },
    { cmd: whichSync("gtime"), flag: "-v", parse: gnuParse, label: "GNU time (gtime)" },
    { cmd: "/usr/bin/time", flag: "-l", parse: bsdParse, label: "BSD time" },
  ];
  for (const c of candidates) {
    if (!c.cmd || !existsSync(c.cmd)) continue;
    const probe = spawnSync(c.cmd, [c.flag, "true"], { encoding: "utf8" });
    if (probe.status === 0 && c.parse(probe.stderr ?? "") != null) {
      return { kind: "time", ...c };
    }
  }
  if (process.platform === "linux" && existsSync("/proc/self/status")) {
    return { kind: "proc", label: "/proc VmHWM poller" };
  }
  return null;
}

// One measured run under the `time` wrapper: exact child ru_maxrss in bytes.
async function rssOnceTime(strategy, runtime, file) {
  const { stderr } = await execFileP(
    strategy.cmd,
    [strategy.flag, runtime.cmd, ...runtime.args, file],
    { maxBuffer: 64 * 1024 * 1024 },
  );
  return strategy.parse(stderr ?? "");
}

// One measured run polling /proc/<pid>/status VmHWM (bytes; null if the
// process exited before any sample landed).
function rssOnceProc(runtime, file) {
  return new Promise((resolvePromise, reject) => {
    let hwm = null;
    let timer = null;
    const child = execFile(
      runtime.cmd,
      [...runtime.args, file],
      { maxBuffer: 64 * 1024 * 1024 },
      (err) => {
        clearInterval(timer);
        if (err) reject(new Error(`${runtime.name} failed on ${file}: ${err.message}`));
        else resolvePromise(hwm);
      },
    );
    const sample = () => {
      try {
        const status = readFileSync(`/proc/${child.pid}/status`, "utf8");
        const m = status.match(/^VmHWM:\s+(\d+) kB/m);
        if (m) hwm = Math.max(hwm ?? 0, Number(m[1]) * 1024);
      } catch {
        // Process already gone — keep whatever high-water mark we saw.
      }
    };
    sample();
    timer = setInterval(sample, 1);
  });
}

// Median peak RSS in bytes over `runs` dedicated runs (null when unmeasurable).
async function measureRss(strategy, runtime, file, runs) {
  const samples = [];
  for (let i = 0; i < runs; i++) {
    const rss =
      strategy.kind === "time"
        ? await rssOnceTime(strategy, runtime, file)
        : await rssOnceProc(runtime, file);
    if (rss != null) samples.push(rss);
  }
  return samples.length ? summarize(samples).median : null;
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

function fmtMs(ms) {
  if (ms == null) return "—";
  if (ms >= 1000) return (ms / 1000).toFixed(2) + "s";
  return ms.toFixed(1) + "ms";
}

function fmtBytes(b) {
  if (b == null) return "—";
  if (b >= 1024 * 1024) return (b / (1024 * 1024)).toFixed(1) + "MiB";
  return (b / 1024).toFixed(0) + "KiB";
}

function pad(s, w) {
  s = String(s);
  return s.length >= w ? s : s + " ".repeat(w - s.length);
}
function padLeft(s, w) {
  s = String(s);
  return s.length >= w ? s : " ".repeat(w - s.length) + s;
}

// Execution-only time per runtime for one workload (median minus that runtime's
// startup baseline), plus which runtime was fastest. The fastest is the
// reference for the "x" slowdown factors. Shared by the text and Markdown
// reporters so they can't drift.
function execTimes(w, rtNames, baselines) {
  const execByName = {};
  for (const n of rtNames) {
    const r = w.byRuntime[n];
    execByName[n] = r ? Math.max(0, r.median - baselines[n]) : null;
  }
  const finite = Object.values(execByName).filter((v) => v != null && v > 0);
  const best = finite.length ? Math.min(...finite) : null;
  let fastestName = "";
  for (const n of rtNames) if (execByName[n] != null && execByName[n] === best) fastestName = n;
  return { execByName, best, fastestName };
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
    const { execByName, best, fastestName } = execTimes(w, rtNames, baselines);
    for (const n of rtNames) {
      const exec = execByName[n];
      if (exec == null) { row += "  " + padLeft("—", COL); continue; }
      const factor = best && exec > 0 ? exec / best : 1;
      const cell = factor <= 1.001 ? fmtMs(exec) : `${fmtMs(exec)} ${factor.toFixed(1)}x`;
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

// Smallest peak RSS across runtimes for one row — the reference for the "x"
// blow-up factors (smaller is better). Shared by the text and Markdown
// reporters so they can't drift.
function rssBest(rssByName, rtNames) {
  const finite = rtNames.map((n) => rssByName[n]).filter((v) => v != null && v > 0);
  const best = finite.length ? Math.min(...finite) : null;
  let smallestName = "";
  for (const n of rtNames) if (rssByName[n] != null && rssByName[n] === best) smallestName = n;
  return { best, smallestName };
}

// The memory table rows: the startup probe (the runtime's floor footprint —
// binary + realm, before any workload allocates) followed by each workload.
// Peak RSS is reported absolute, not startup-subtracted: unlike wall-clock,
// RSS does not subtract linearly (allocators reuse the startup pages).
function memoryRows(workloads, startupRss) {
  return [{ name: "(startup)", rssByRuntime: startupRss }, ...workloads.map((w) => ({ name: w.name, rssByRuntime: w.rssByRuntime }))];
}

function printMemoryTable(workloads, rtNames, mem) {
  const COL = 13;
  const nameW = Math.max(12, ...workloads.map((w) => w.name.length));
  console.log(
    `\nPeak memory (subprocess max RSS, median of ${mem.runs} dedicated run(s); via ${mem.strategy.label})`,
  );
  console.log("");
  let header = pad("workload", nameW);
  for (const n of rtNames) header += "  " + padLeft(n, COL);
  header += "  " + padLeft("smallest", 9);
  console.log(header);
  console.log("-".repeat(header.length));
  for (const row of memoryRows(workloads, mem.startupRss)) {
    let line = pad(row.name, nameW);
    const { best, smallestName } = rssBest(row.rssByRuntime, rtNames);
    for (const n of rtNames) {
      const rss = row.rssByRuntime[n];
      if (rss == null) { line += "  " + padLeft("—", COL); continue; }
      const factor = best ? rss / best : 1;
      const cell = factor <= 1.001 ? fmtBytes(rss) : `${fmtBytes(rss)} ${factor.toFixed(1)}x`;
      line += "  " + padLeft(cell, COL);
    }
    line += "  " + padLeft(smallestName, 9);
    console.log(line);
  }
}

// Stable HTML marker so CI can find-and-update its single sticky comment
// instead of posting a new one every run.
const MARKDOWN_MARKER = "<!-- chidori-js-benchmarks -->";

// Render the results as a Markdown report (used for the PR comment).
function renderMarkdown(workloads, runtimes, baselines, opts, mem) {
  const rtNames = runtimes.map((r) => r.name);
  const lines = [];
  lines.push(MARKDOWN_MARKER);
  lines.push("## chidori-js cross-runtime benchmarks");
  lines.push("");
  lines.push(
    `Same JS workloads run under **chidori-js**, **Node.js**, and **Bun** ` +
      `(${opts.runs} timed run(s) + ${opts.warmup} warmup each, median reported). ` +
      `All workloads cross-checked to produce identical results.`,
  );
  lines.push("");
  lines.push(
    "**Startup baselines:** " +
      rtNames.map((n) => `${n} ${fmtMs(baselines[n])}`).join(" · "),
  );
  lines.push("");

  // Table 1: execution-only (startup subtracted), with slowdown factors.
  lines.push("### Execution-only time (startup baseline subtracted)");
  lines.push("");
  lines.push(`| workload | ${rtNames.join(" | ")} | fastest |`);
  lines.push(`|${"---|".repeat(rtNames.length + 2)}`);
  for (const w of workloads) {
    const { execByName, best, fastestName } = execTimes(w, rtNames, baselines);
    const cells = rtNames.map((n) => {
      const exec = execByName[n];
      if (exec == null) return "—";
      const factor = best && exec > 0 ? exec / best : 1;
      const txt = factor <= 1.001 ? `**${fmtMs(exec)}**` : `${fmtMs(exec)} (${factor.toFixed(1)}×)`;
      return txt;
    });
    const flag = w.agree ? "" : " ⚠️";
    lines.push(`| ${w.name}${flag} | ${cells.join(" | ")} | ${fastestName} |`);
  }
  lines.push("");

  // Table 2: raw total wall-clock including startup.
  lines.push("### Total time including startup (raw wall-clock)");
  lines.push("");
  lines.push(`| workload | ${rtNames.join(" | ")} |`);
  lines.push(`|${"---|".repeat(rtNames.length + 1)}`);
  for (const w of workloads) {
    const cells = rtNames.map((n) => {
      const r = w.byRuntime[n];
      return r ? fmtMs(r.median) : "—";
    });
    lines.push(`| ${w.name} | ${cells.join(" | ")} |`);
  }
  lines.push("");

  // Table 3: peak memory (max RSS), smallest bolded, blow-up factors shown.
  if (mem) {
    lines.push(
      `### Peak memory (subprocess max RSS, median of ${mem.runs} dedicated run(s))`,
    );
    lines.push("");
    lines.push(`| workload | ${rtNames.join(" | ")} | smallest |`);
    lines.push(`|${"---|".repeat(rtNames.length + 2)}`);
    for (const row of memoryRows(workloads, mem.startupRss)) {
      const { best, smallestName } = rssBest(row.rssByRuntime, rtNames);
      const cells = rtNames.map((n) => {
        const rss = row.rssByRuntime[n];
        if (rss == null) return "—";
        const factor = best ? rss / best : 1;
        return factor <= 1.001 ? `**${fmtBytes(rss)}**` : `${fmtBytes(rss)} (${factor.toFixed(1)}×)`;
      });
      lines.push(`| ${row.name} | ${cells.join(" | ")} | ${smallestName} |`);
    }
    lines.push("");
  }

  lines.push(
    "<sub>Numbers are machine- and load-dependent (shared CI runner) — read them " +
      "as ratios, not absolutes. chidori-js is an interpreter, so it trails the " +
      "V8/JSC JITs on compute but starts far faster and in far less memory. " +
      "A ⚠️ marks a workload whose result disagreed across runtimes.</sub>",
  );
  return lines.join("\n") + "\n";
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

  const memStrategy = opts.memory ? detectMemoryStrategy() : null;
  if (opts.memory && !memStrategy) {
    process.stderr.write(
      "warning: no peak-RSS measurement available on this platform (need GNU/BSD `time` or Linux /proc) — memory table skipped\n",
    );
  }

  console.log(
    `chidori-js cross-runtime benchmarks\n` +
      `runtimes: ${runtimes.map((r) => r.name).join(", ")}  |  ` +
      `runs: ${opts.runs}  warmup: ${opts.warmup}  workloads: ${files.length}  |  ` +
      `memory: ${memStrategy ? memStrategy.label : "off"}`,
  );

  // Startup baseline per runtime (median of a few extra runs — it's cheap).
  // With memory on, also probe startup peak RSS: the runtime's floor
  // footprint before any workload allocates.
  const baselines = {};
  const startupRss = {};
  const startupFile = join(WORKLOAD_DIR, "startup.js");
  for (const rt of runtimes) {
    const { median } = await timeWorkload(rt, startupFile, Math.max(opts.runs, 5), 1);
    baselines[rt.name] = median;
    if (memStrategy) {
      startupRss[rt.name] = await measureRss(memStrategy, rt, startupFile, opts.memRuns);
    }
  }

  const workloads = [];
  let mismatches = 0;
  for (const f of files) {
    const file = join(WORKLOAD_DIR, f);
    const name = f.replace(/\.js$/, "");
    process.stderr.write(`  ${name} ... `);
    const byRuntime = {};
    const rssByRuntime = {};
    const results = new Set();
    for (const rt of runtimes) {
      const r = await timeWorkload(rt, file, opts.runs, opts.warmup);
      byRuntime[rt.name] = r;
      if (memStrategy) {
        rssByRuntime[rt.name] = await measureRss(memStrategy, rt, file, opts.memRuns);
      }
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
    workloads.push({ name, byRuntime, rssByRuntime, agree, result: [...results][0] });
  }

  const mem = memStrategy ? { strategy: memStrategy, startupRss, runs: opts.memRuns } : null;

  printTable(workloads, runtimes, baselines);
  if (mem) printMemoryTable(workloads, runtimes.map((r) => r.name), mem);

  if (mismatches > 0) {
    console.log(
      `\n⚠️  ${mismatches} workload(s) disagreed across runtimes — timings above are not comparable for those rows.`,
    );
  }

  if (opts.json) {
    const payload = {
      generatedAt: new Date().toISOString(),
      options: { runs: opts.runs, warmup: opts.warmup, memRuns: opts.memRuns },
      runtimes: runtimes.map((r) => ({ name: r.name, cmd: r.cmd })),
      baselinesMs: baselines,
      memory: mem
        ? { strategy: mem.strategy.label, startupRssBytes: startupRss }
        : null,
      workloads: workloads.map((w) => ({
        name: w.name,
        agree: w.agree,
        result: w.result,
        runtimes: Object.fromEntries(
          Object.entries(w.byRuntime).map(([n, r]) => [
            n,
            {
              median: r.median,
              min: r.min,
              max: r.max,
              mean: r.mean,
              samples: r.samples,
              maxRssBytes: mem ? (w.rssByRuntime[n] ?? null) : null,
            },
          ]),
        ),
      })),
    };
    writeFileSync(opts.json, JSON.stringify(payload, null, 2));
    console.log(`\nwrote ${opts.json}`);
  }

  if (opts.markdown) {
    writeFileSync(opts.markdown, renderMarkdown(workloads, runtimes, baselines, opts, mem));
    console.log(`\nwrote ${opts.markdown}`);
  }

  process.exit(mismatches > 0 ? 1 : 0);
}

main().catch((err) => {
  console.error("\nerror: " + err.message);
  process.exit(1);
});
