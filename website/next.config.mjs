import { createMDX } from 'fumadocs-mdx/next';

const withMDX = createMDX();

/** @type {import('next').NextConfig} */
const config = {
  output: 'export',
  trailingSlash: true,
  // GitHub Pages serves project sites under /<repo>; CI sets /chidori.
  basePath: process.env.DOCS_BASE_PATH || '',
  reactStrictMode: true,
  images: { unoptimized: true },
};

export default withMDX(config);
