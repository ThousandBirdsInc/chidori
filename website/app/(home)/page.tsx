import Link from 'next/link';
import { REPO_URL } from '@/lib/layout.shared';

const FEATURES = [
  {
    title: 'Replay any run with zero LLM calls',
    body: 'The call log is a deterministic record. Re-run the same code against it and every prompt, tool, and HTTP call returns its recorded result instantly — no tokens spent, identical output.',
    href: '/docs/replay',
  },
  {
    title: 'Survive crashes and restarts',
    body: 'Runs are checkpointed at every host safepoint. Kill the process mid-run and resume exactly where it left off, in a brand-new process.',
    href: '/docs/durable-storage',
  },
  {
    title: 'Pause for humans, without a live process',
    body: 'chidori.input() and named signals suspend the run to disk. A human or another agent answers minutes or days later and the run picks up exactly where it stopped.',
    href: '/docs/signals',
  },
  {
    title: 'Check in a checkpoint as a test',
    body: 'Commit a recorded run to git and assert the agent’s behavior hasn’t drifted — a full integration test that costs $0 and runs in milliseconds.',
    href: '/docs/value-checkpoints',
  },
  {
    title: 'One Rust binary, no runtime dependencies',
    body: 'An embedded pure-Rust JavaScript engine runs your agents — no Node, no Deno, no V8. TypeScript and Python SDKs talk to it over HTTP with no native bindings.',
    href: '/docs/architecture',
  },
  {
    title: 'Structural prompt caching built in',
    body: 'Stable prefixes are auto-marked for the provider cache, and replay pays nothing at all.',
    href: '/docs/context-management',
  },
];

export default function HomePage() {
  return (
    <main className="flex flex-1 flex-col">
      <section className="mx-auto w-full max-w-5xl px-6 pt-20 pb-16 md:pt-28">
        <h1 className="max-w-3xl text-4xl font-semibold tracking-tight text-balance md:text-5xl">
          The agent framework where every run is durable, replayable, and
          resumable by default.
        </h1>
        <p className="mt-6 max-w-2xl text-lg text-fd-muted-foreground">
          Write agents as plain async TypeScript on a Rust core. Every side
          effect flows through the runtime as a recorded host call, so any run
          can be checkpointed to disk, replayed for byte-identical output with
          zero LLM calls, and resumed from any pause — even in a new process
          after a crash.
        </p>
        <div className="mt-8 flex flex-wrap gap-3">
          <Link
            href="/docs/getting-started"
            className="rounded-lg bg-fd-foreground px-5 py-2.5 text-sm font-medium text-fd-background transition-opacity hover:opacity-85"
          >
            Get Started
          </Link>
          <Link
            href="/docs"
            className="rounded-lg border border-fd-border px-5 py-2.5 text-sm font-medium transition-colors hover:bg-fd-accent"
          >
            Read the Docs
          </Link>
          <a
            href={REPO_URL}
            rel="noreferrer noopener"
            className="rounded-lg border border-fd-border px-5 py-2.5 text-sm font-medium transition-colors hover:bg-fd-accent"
          >
            GitHub
          </a>
        </div>
      </section>

      <section className="mx-auto w-full max-w-5xl px-6 pb-24">
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {FEATURES.map((f) => (
            <Link
              key={f.href + f.title}
              href={f.href}
              className="rounded-xl border border-fd-border/70 bg-fd-card p-5 transition-colors hover:border-fd-border hover:bg-fd-accent/50"
            >
              <h2 className="text-sm font-semibold">{f.title}</h2>
              <p className="mt-2 text-sm leading-relaxed text-fd-muted-foreground">
                {f.body}
              </p>
            </Link>
          ))}
        </div>
      </section>
    </main>
  );
}
