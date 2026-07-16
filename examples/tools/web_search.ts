import type { Chidori, ToolDefinition } from "chidori:agent";

export const tool: ToolDefinition = {
  name: "web_search",
  description:
    "Search the web for a short query using the keyless DuckDuckGo Instant " +
    "Answer API. Returns an abstract (when DuckDuckGo has one) plus related " +
    "topic links. Lightweight and key-free — swap in your preferred search " +
    "API for production use.",
  parameters: {
    type: "object",
    properties: {
      query: { type: "string", description: "Search query" },
    },
    required: ["query"],
  },
};

export async function run(args: { query: string }, chidori: Chidori) {
  const url =
    "https://api.duckduckgo.com/?format=json&no_html=1&skip_disambig=1&q=" +
    encodeURIComponent(args.query);
  await chidori.log("Running web_search", { query: args.query });

  // The runtime's captured fetch: policy-gated, journaled, replayable.
  const res = await fetch(url);
  if (!res.ok) {
    return { query: args.query, error: `search returned HTTP ${res.status}`, results: [] };
  }
  const data = await res.json();

  const results: { title: string; url: string; snippet: string }[] = [];
  if (data.AbstractText) {
    results.push({
      title: data.Heading || args.query,
      url: data.AbstractURL || "",
      snippet: data.AbstractText,
    });
  }
  const flatten = (topics: any[]) => {
    for (const t of topics ?? []) {
      if (t.Topics) {
        flatten(t.Topics);
      } else if (t.FirstURL && t.Text) {
        results.push({ title: t.Text.split(" - ")[0], url: t.FirstURL, snippet: t.Text });
      }
    }
  };
  flatten(data.RelatedTopics);

  return { query: args.query, results: results.slice(0, 8) };
}
