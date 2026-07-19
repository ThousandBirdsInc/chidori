import { docs } from '@/.source';
import { loader } from 'fumadocs-core/source';

// fumadocs-mdx 11.10 exposes `files` as a lazy function, while the
// fumadocs-core 15.8 loader expects a plain array — resolve it here so
// either shape works.
const mdxSource = docs.toFumadocsSource();
const rawFiles: unknown = mdxSource.files;
const files =
  typeof rawFiles === 'function' ? (rawFiles as () => unknown[])() : rawFiles;

export const source = loader({
  baseUrl: '/docs',
  source: { files } as unknown as typeof mdxSource,
});