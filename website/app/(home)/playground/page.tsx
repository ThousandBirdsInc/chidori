import type { Metadata } from 'next';
import { PlaygroundClient } from './playground-client';

export const metadata: Metadata = {
  title: 'Playground — Chidori',
  description:
    'Chat with a chidori agent running entirely in your browser: the pure-Rust engine compiled to WebAssembly, with tools, generative UI, and docs-grounded answers.',
};

export default function PlaygroundPage() {
  return (
    <main className="mx-auto w-full max-w-3xl flex-1 px-6 py-12">
      <h1 className="text-3xl font-semibold tracking-tight md:text-4xl">
        Playground
      </h1>
      <p className="mt-3 text-fd-muted-foreground">
        A chidori agent running entirely in this tab — ask it about chidori, or
        hand it a tool. Every effect is journaled: reload mid-conversation and
        it resumes, rewind any turn, or branch into an alternate timeline.
      </p>
      <PlaygroundClient />
    </main>
  );
}
