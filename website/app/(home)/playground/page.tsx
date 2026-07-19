import type { Metadata } from 'next';
import { PlaygroundClient } from './playground-client';

export const metadata: Metadata = {
  title: 'Playground — Chidori',
  description:
    'Run a durable chidori agent entirely in your browser: the pure-Rust engine compiled to WebAssembly, with suspend, resume, and offline replay.',
};

export default function PlaygroundPage() {
  return (
    <main className="mx-auto w-full max-w-5xl flex-1 px-6 py-12">
      <h1 className="text-3xl font-semibold tracking-tight md:text-4xl">
        Playground
      </h1>
      <p className="mt-4 max-w-3xl text-fd-muted-foreground">
        A <b>client-side-only</b> chidori agent — the pure-Rust engine and its
        durable replay runtime compiled to WebAssembly, running in this tab.
        The agent below suspends at <code>chidori.input()</code>: the run is
        saved to <code>localStorage</code>, survives a page reload, resumes
        exactly where it stopped, and replays offline with zero live host
        calls. No server, no keys — the LLM is a deterministic mock.
      </p>
      <PlaygroundClient />
    </main>
  );
}
