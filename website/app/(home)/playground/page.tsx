import type { Metadata } from 'next';
import { PlaygroundClient } from './playground-client';

export const metadata: Metadata = {
  title: 'Playground — Chidori',
  description:
    'Chat with a chidori agent running entirely in your browser: the pure-Rust engine compiled to WebAssembly, with tools, generative UI, docs-grounded answers — and the ability to rewrite its own code mid-conversation.',
};

// The HomeLayout already renders the page's <main> landmark, so this wrapper
// is a plain div. The header stays compact on phones — the chat panel below
// sizes itself against the viewport and needs the room.
export default function PlaygroundPage() {
  return (
    <div className="mx-auto w-full max-w-3xl px-4 pt-6 pb-4 sm:px-6 sm:pt-10 sm:pb-10">
      <h1 className="text-2xl font-semibold tracking-tight sm:text-3xl md:text-4xl">
        Playground
      </h1>
      <p className="mt-3 hidden text-fd-muted-foreground sm:block">
        A chidori agent running entirely in this tab — ask it about chidori,
        hand it a tool, or ask it to rewrite its own code. Every effect is
        journaled: reload mid-conversation and it resumes, rewind any turn,
        branch into an alternate timeline — or hot-swap the agent&apos;s
        implementation and replay the same history against the new code.
      </p>
      <p className="mt-1.5 text-sm text-fd-muted-foreground sm:hidden">
        A live agent in this tab — journaled, rewindable, branchable, and able
        to rewrite its own code.
      </p>
      <PlaygroundClient />
    </div>
  );
}
