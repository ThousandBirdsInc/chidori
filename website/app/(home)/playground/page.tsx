import type { Metadata } from 'next';
import { PlaygroundClient } from './playground-client';

export const metadata: Metadata = {
  title: 'Playground — Chidori',
  description:
    'Chat with a chidori agent running entirely in your browser: the pure-Rust engine compiled to WebAssembly, with tools, generative UI, docs-grounded answers — and the ability to rewrite its own code mid-conversation.',
};

export default function PlaygroundPage() {
  return (
    <main className="mx-auto w-full max-w-3xl flex-1 px-4 py-8 sm:px-6 sm:py-12">
      <h1 className="text-3xl font-semibold tracking-tight md:text-4xl">
        Playground
      </h1>
      <p className="mt-3 text-fd-muted-foreground">
        A chidori agent running entirely in this tab — ask it about chidori,
        hand it a tool, or ask it to rewrite its own code. Every effect is
        journaled: reload mid-conversation and it resumes, rewind any turn,
        branch into an alternate timeline — or hot-swap the agent&apos;s
        implementation and replay the same history against the new code.
      </p>
      <PlaygroundClient />
    </main>
  );
}
