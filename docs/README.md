# Chidori documentation

This directory mixes two audiences. **Using Chidori** is the path for agent
authors and operators; **Engineering notes** are internal design records —
useful history and rationale, but not tutorials, and some describe work that
was completed, retired, or superseded.

## Using Chidori

Start here, roughly in order:

| Doc | What it covers |
|---|---|
| [getting-started.md](./getting-started.md) | Install, first agent, first replay |
| [core-concepts.md](./core-concepts.md) | Host calls, the call log, safepoints |
| [replay.md](./replay.md) | Record, replay, resume, divergence rules |
| [running-modes.md](./running-modes.md) | `run` vs `serve`, policy profiles, `--trusted` |
| [signals.md](./signals.md) | Named signals: pause for humans or other agents |
| [branching-execution.md](./branching-execution.md) | `chidori.branch` sub-runs |
| [actors.md](./actors.md) | Supervised, message-passing agent processes |
| [detached-agents.md](./detached-agents.md) | Long-lived agents outside a session |
| [context-management.md](./context-management.md) | Conversation and context windows |
| [value-checkpoints.md](./value-checkpoints.md) | `durableStep`: bounding replay cost |
| [durable-storage.md](./durable-storage.md) | Run persistence, time travel (`--until-seq`) |
| [package-management.md](./package-management.md) | Imports, `node:` builtins, npm packages |
| [sandbox-model.md](./sandbox-model.md) | The security model and its guarantees |
| [deployment.md](./deployment.md) | Serving agents in production |

## Engineering notes (internal)

Design records for contributors. Status headers inside each file are
authoritative — several document retired or superseded work:

- [architecture.md](./architecture.md) — engine + runtime layering
- [conformance.md](./conformance.md) — Test262 methodology and CI gate
- [captured-effects-vfs-crypto-timers.md](./captured-effects-vfs-crypto-timers.md) — captured-effect surfaces
- [interpreter-optimization.md](./interpreter-optimization.md) — measured optimization phases
- [js-performance-roadmap.md](./js-performance-roadmap.md) — profiling data and roadmap
- [js-object-shapes-design.md](./js-object-shapes-design.md) — hidden-class design (implemented)
- [jit.md](./jit.md) — closure-threading JIT experiment (**retired**; kept as data)
- [os-isolation-plan.md](./os-isolation-plan.md) — process isolation design
- [resume-performance.md](./resume-performance.md) — resume cost analysis
- [dom-runtime-prototype.md](./dom-runtime-prototype.md) — DOM runtime prototype
- [ai-sdk-gap-analysis.md](./ai-sdk-gap-analysis.md) — feature comparison vs Vercel AI SDK
- [consumer-usability-review.md](./consumer-usability-review.md) — round 1: building a real agent on 3.6.0 (linear path)
- [consumer-usability-review-2.md](./consumer-usability-review-2.md) — round 2: the multi-agent surface (actors, branches, detached agents) under failure
- [consumer-usability-review-3.md](./consumer-usability-review-3.md) — round 3: the everyday-agent surface as a daily driver
- [consumer-usability-review-4.md](./consumer-usability-review-4.md) — round 4: the day-2 surface (npm packages, durable store, hydration, time travel, `verify`)
- [consumer-usability-review-5.md](./consumer-usability-review-5.md) — round 5: shipping to users (`serve` in production posture, SSE streaming, multiplayer signals under crashes, SDK-as-client, webhooks)
- [branching-execution.md](./branching-execution.md) — also doubles as the branching design record
- [rust-style-guide.md](./rust-style-guide.md) — contributor conventions
- [releasing.md](./releasing.md) — release train and versioning
