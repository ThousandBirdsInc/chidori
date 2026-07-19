import { source } from '@/lib/source';
import { DocsBody, DocsPage } from 'fumadocs-ui/page';
import { notFound } from 'next/navigation';
import { getMDXComponents } from '@/mdx-components';
import type { Metadata } from 'next';

export default async function Page(props: {
  params: Promise<{ slug?: string[] }>;
}) {
  const params = await props.params;
  const page = source.getPage(params.slug);
  if (!page) notFound();

  const MDX = page.data.body;

  return (
    <DocsPage
      // The markdown keeps its own H1 (the files must stay readable on
      // GitHub), so the page renders the body only and drops the H1 from
      // the table of contents.
      toc={page.data.toc.filter((item) => item.depth > 1)}
      full={page.data.full}
      editOnGithub={{
        owner: 'ThousandBirdsInc',
        repo: 'chidori',
        sha: 'main',
        path: `docs/${page.path}`,
      }}
    >
      <DocsBody>
        <MDX components={getMDXComponents()} />
      </DocsBody>
    </DocsPage>
  );
}

export function generateStaticParams() {
  return source.generateParams();
}

export async function generateMetadata(props: {
  params: Promise<{ slug?: string[] }>;
}): Promise<Metadata> {
  const params = await props.params;
  const page = source.getPage(params.slug);
  if (!page) notFound();

  return {
    title: page.data.title,
    description: page.data.description,
  };
}
