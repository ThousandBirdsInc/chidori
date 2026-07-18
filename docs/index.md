---
layout: home

hero:
  name: Chidori
  text: Durable, replayable, resumable agents
  tagline: >-
    Write agents as plain async TypeScript on a Rust core. Every side effect
    flows through the runtime as a recorded host call, so any run can be
    checkpointed to disk, replayed for byte-identical output with zero LLM
    calls, and resumed from any pause — even in a new process after a crash.
  actions:
    - theme: brand
      text: Get Started
      link: /getting-started
    - theme: alt
      text: Core Concepts
      link: /core-concepts
    - theme: alt
      text: GitHub
      link: https://github.com/ThousandBirdsInc/chidori

features:
  - icon: 🔁
    title: Replay any run with zero LLM calls
    details: >-
      The call log is a deterministic record. Re-run the same code against it
      and every prompt, tool, and HTTP call returns its recorded result
      instantly — no tokens spent, identical output.
    link: /replay
  - icon: 💾
    title: Survive crashes and restarts
    details: >-
      Runs are checkpointed at every host safepoint. Kill the process mid-run
      and resume exactly where it left off, in a brand-new process.
    link: /durable-storage
  - icon: 🧑‍⚖️
    title: Pause for humans, without a live process
    details: >-
      chidori.input() and named signals suspend the run to disk. A human or
      another agent answers minutes or days later and the run picks up exactly
      where it stopped.
    link: /signals
  - icon: 🧪
    title: Check in a checkpoint as a test
    details: >-
      Commit a recorded run to git and assert the agent's behavior hasn't
      drifted — a full integration test that costs $0 and runs in
      milliseconds.
    link: /value-checkpoints
  - icon: 📦
    title: One Rust binary, no runtime dependencies
    details: >-
      An embedded pure-Rust JavaScript engine runs your agents — no Node, no
      Deno, no V8. TypeScript and Python SDKs talk to it over HTTP with no
      native bindings.
    link: /architecture
  - icon: ⚡
    title: Structural prompt caching built in
    details: >-
      Stable prefixes are auto-marked for the provider cache, and replay pays
      nothing at all.
    link: /context-management
---
