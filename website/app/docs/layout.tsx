import { DocsLayout } from 'fumadocs-ui/layouts/docs';
import type { ReactNode } from 'react';
import { baseOptions } from '@/lib/layout.shared';
import { source } from '@/lib/source';
import { RunnerPanel } from './runner/runner-panel';

export default function Layout({ children }: { children: ReactNode }) {
  return (
    <DocsLayout tree={source.pageTree} {...baseOptions()}>
      {children}
      {/* The example-runner side panel: mounted once for all docs pages,
          opened by the Run button on runnable code blocks. It also finishes
          the site-wide OpenRouter PKCE login when a page load is the
          redirect back from openrouter.ai. */}
      <RunnerPanel />
    </DocsLayout>
  );
}
