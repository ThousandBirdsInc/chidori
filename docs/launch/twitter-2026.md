# Twitter/X launch thread (2026)

11 tweets. Attach `replay-demo-crt.mp4` (in this folder) to tweet 1 — that's
the CRT cut; `replay-demo.mp4` is the earlier flat version. Attach an MP4
conversion of `docs/media/react-agent.svg` to tweet 6 (X rejects SVG).
Tweets 5 and 10 run slightly past 280 characters and need X Premium or a trim.

---

**1/** *(attach replay-demo-crt.mp4)*

Three years ago I open-sourced an AI agent framework. I just rewrote nearly all of it, down to a from-scratch JavaScript engine in Rust.

The result: any agent run replays byte-for-byte, with zero model calls. 🧵

**2/**

The 2023 version was a declarative framework. Reactive graph, embedded interpreter, time-travel debugging, evals.

The bet was right. Reproduce what an agent did five steps back and you can make it reliable. The design was wrong. Nobody thinks in reactive nodes.

**3/**

And the DSL had a bigger problem. Agents get generated now, and models write idiomatic TypeScript far better than any custom graph format.

A constrained DSL looked like a safety feature. Once code writes your agents, it's a worse compile target.

**4/**

So agents are now plain async TypeScript functions. Every model call, tool call, and HTTP request goes through the runtime and gets recorded.

Record everything and you can replay everything. Replay is only worth something if it's exact.

**5/**

Exact meant owning the engine. So Chidori ships its own, built from scratch in Rust. oxc parser, bytecode, stack VM, zero unsafe. 99% of the Test262 tests it executes pass, and the skips are listed in the repo.

The runtime owns the clock, randomness, and every I/O call the agent sees.

**6/** *(attach MP4 converted from docs/media/react-agent.svg)*

Then I ran React on it. Unmodified React 18.

An agent iterates on a component against its own DOM tests, forks a variant, and replays its earlier model calls for free. Human feedback gets recorded too, so a replay never re-asks the person.

**7/**

One person built this by directing coding agents. The engine, the Test262 grind, the runtime.

I trusted code I didn't type because every change ran through conformance gates and every run was replayable. I used the exact discipline this framework sells. It held.

**8/**

Exact replay in practice:

- crash mid-run, resume in a fresh process
- pause for a human, pick up days later
- race strategies across real OS threads, still deterministic
- commit a run to git as an integration test that costs $0 and runs in ms

**9/**

Next: agents that build their own tools. A tool is just TypeScript, and agents write good TypeScript. Solve a problem the hard way once, factor it into a typed tool, reach for it forever after.

The whole architecture points here.

**10/**

Temporal replays journaled histories too. It also makes you split workflows from activities and keep half your code deterministic by discipline. LangGraph checkpoints between nodes. eve records traces of agents holding a real shell.

Chidori puts determinism in the engine. There is nothing to get wrong.

**11/**

The trade I made: no JIT, so raw compute runs 10-40x slower than Node. An agent spends its life waiting on model calls, where that gap disappears. Startup beats Node by 10x.

One binary. Apache-2.0. chidori login, no API key needed.

https://github.com/ThousandBirdsInc/chidori

Show me an agent that can't fit through a recorded-side-effect boundary.
