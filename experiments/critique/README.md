# Chidori end-user critique experiments

Hands-on experiments run against a from-source build (`cargo build --release`)
to evaluate Chidori's core claims as an end user would experience them. No LLM
API key is used anywhere — every experiment exercises the durability layer on
deterministic, non-LLM behaviors, which is exactly the setup
`examples/record-replay/` recommends.

| # | File | Claim under test |
|---|------|------------------|
| E1 | *(observational)* | "One self-contained binary" — build-from-source friction |
| E2 | `../../examples/agents/hello.ts` | Zero-setup hello world, cold-start latency |
| E3 | `../../examples/agents/tool_use.ts` | Ask-by-default policy: gated effects fail closed without a TTY |
| E4 | `exactly_once_probe.ts` + `tools/side_effect.ts` | Replay re-executes **zero** side effects (on-disk ledger as ground truth) |
| E5 | `determinism_probe.ts` | `Date.now()` / `Math.random()` / timers are captured; replay is byte-identical |
| E6 | `js_conformance_probe.ts` | The from-scratch JS engine covers the modern-JS surface agent authors reach for |
| E7 | `error_quality_bad_runtime.ts`, `error_quality_bad_syntax.ts` | Error message quality: stack traces, line/column info |
| E8 | `perf_compute.ts` vs `perf_compute_node.mjs` | Interpreter throughput vs Node.js on identical pure-compute work |
| E9 | `pause_resume_probe.ts` | A run paused on `chidori.input()` survives process death and resumes in a new process |

Results are recorded in `RESULTS.md` and visualized in the accompanying
artifact.
