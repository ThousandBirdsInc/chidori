# Twitter/X launch thread (2026)

11 tweets. Attachments: `agent-code.png` on tweet 1, `replay-demo-crt.mp4`
(the code -> run -> kill -> resume CRT cut) on tweet 4, and an MP4 conversion
of `docs/media/react-agent.svg` on tweet 7 (X rejects SVG). A few tweets run
past 280 characters and need X Premium or a trim.

---

**1/** *(attach agent-code.png)*

This is a complete production agent.

The prompt, the tool call, the human approval gate — the whole thing is one TypeScript function. There is no graph or DSL underneath. This is the entire authoring model.

I spent three years getting it down to this 🧵

**2/**

Frameworks made agents feel like framework code. Model calls buried three abstraction layers deep, and when something breaks you end up debugging the framework.

So the design goal for the rewrite was simple: disappear. If you can write a function, you already know Chidori.

**3/**

That one function reaches everything through the chidori object: prompts, typed tools (plain TS files), MCP servers, human approvals, multi-turn chat, parallel branches.

Anthropic, OpenAI, OpenRouter, LiteLLM. chidori login gets you running with no API key at all.

**4/** *(attach replay-demo-crt.mp4)*

And the plain function comes with superpowers. Kill the process mid-run. Resume in a new process, at the exact step, byte-identical.

Your function contains no retry logic, no state machine, no checkpointing code. The runtime absorbs all of it.

**5/**

It works because the agent never touches the world directly. Every prompt, tool call, and fetch goes through the runtime and gets recorded.

A run becomes a journal you can resume after a crash, fork into variants, or replay to debug. Commit one to git and you have a regression test.

**6/**

Exact replay required owning the clock, randomness, and every I/O call. So Chidori ships its own JavaScript engine, built from scratch in Rust.

99% of the Test262 tests it executes pass, and the skips are listed in the repo. One binary. No Node, no cluster, nothing to deploy onto.

**7/** *(attach MP4 converted from docs/media/react-agent.svg)*

Then I ran React on it. Unmodified React 18.

An agent iterates on a component against its own DOM tests, forks a variant, and replays its earlier model calls for free. Human feedback gets recorded too, so a replay never re-asks the person.

**8/**

One person built this by directing coding agents. The engine, the Test262 grind, the runtime.

I trusted code I didn't type because every change ran through conformance gates and every run was replayable. I used the exact discipline this framework sells. It held.

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
