import { createMDX } from 'fumadocs-mdx/next';

const withMDX = createMDX();

/** @type {import('next').NextConfig} */
const config = {
  output: 'export',
  trailingSlash: true,
  // GitHub Pages serves project sites under /<repo>; CI sets /chidori.
  basePath: process.env.DOCS_BASE_PATH || '',
  // Client code (the playground) loads the wasm assets from /public at
  // runtime, so it needs the base path inlined where next/link isn't involved.
  env: { NEXT_PUBLIC_BASE_PATH: process.env.DOCS_BASE_PATH || '' },
  reactStrictMode: true,
  images: { unoptimized: true },
};

export default withMDX(config);
