import defaultMdxComponents from 'fumadocs-ui/mdx';
import type { MDXComponents } from 'mdx/types';
import { RunnablePre } from '@/app/docs/runner/runnable-pre';

export function getMDXComponents(components?: MDXComponents): MDXComponents {
  return {
    ...defaultMdxComponents,
    // Code blocks render through the runner-aware wrapper: blocks the
    // build-time analysis proved executable in the browser sandbox get a
    // "Run" button that opens the example-runner side panel.
    pre: RunnablePre,
    ...components,
  };
}
