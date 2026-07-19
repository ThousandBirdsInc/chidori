import { source } from '@/lib/source';
import { createFromSource } from 'fumadocs-core/search/server';

// Static export: the search index is generated at build time and served as
// a static file that the client-side worker queries.
export const revalidate = false;

export const { staticGET: GET } = createFromSource(source, {
  // The default id scheme is the bare page URL, and section documents get
  // `${id}-${n}` — which collides with sibling pages whose slugs end in a
  // number (consumer-usability-review vs consumer-usability-review-2).
  // Suffix the page id with '#' so derived section ids can never equal
  // another page's id.
  buildIndex(page) {
    return {
      title: page.data.title,
      description: page.data.description,
      url: page.url,
      id: `${page.url}#`,
      structuredData: page.data.structuredData,
    };
  },
});
