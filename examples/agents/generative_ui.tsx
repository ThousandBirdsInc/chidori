import type { Chidori } from "chidori";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";

/**
 * Generative agent UI: the model *generates the interface*, and it renders
 * through the same journaled host boundary Chidori uses for `prompt`, `tool`,
 * and `fetch`. See `docs/design/generative-agent-ui.md`.
 *
 * The shape is schema-fill, not code-gen: the model returns a typed JSON
 * `UiSpec`, and a trusted `Screen` component renders it the same way every time.
 * So a generated screen is a *pure function of a journaled spec* — same code +
 * same call log => byte-identical DOM, for zero model calls on replay.
 *
 *   prompt() ──▶ UiSpec (JSON, journaled)
 *               Screen(spec) ──▶ react-dom/server ──▶ innerHTML ──▶ DOM
 *               chidori.renderDOM() ──▶ durable dom_render effect
 *
 * Run (needs an LLM provider configured):
 *   chidori run examples/agents/generative_ui.tsx \
 *     --input description="a pricing card for a Pro plan at \$29/mo"
 */

/** The contract the model fills. The renderer below is the only thing that
 *  turns this into DOM, so the model chooses content within a fixed structure. */
interface UiSpec {
  title: string;
  subtitle?: string;
  /** Bulleted feature/detail lines. */
  features?: string[];
  /** Small labelled chips (e.g. a price, a status). */
  badges?: { label: string; value: string }[];
  /** Primary call-to-action button text. */
  cta?: string;
}

/** Trusted, deterministic renderer. The model never produces this — only the
 *  `UiSpec` it consumes. */
function Screen(props: { spec: UiSpec }): React.ReactElement {
  const { spec } = props;
  const h = React.createElement;
  return (
    <div className="screen">
      <h1>{spec.title}</h1>
      {spec.subtitle ? <p className="subtitle">{spec.subtitle}</p> : null}
      {spec.badges && spec.badges.length > 0 ? (
        <dl className="badges">
          {spec.badges.map((b, i) =>
            h(React.Fragment, { key: i }, h("dt", null, b.label), h("dd", null, b.value)),
          )}
        </dl>
      ) : null}
      {spec.features && spec.features.length > 0 ? (
        <ul className="features">
          {spec.features.map((f, i) => (
            <li key={i}>{f}</li>
          ))}
        </ul>
      ) : null}
      {spec.cta ? <button>{spec.cta}</button> : null}
    </div>
  );
}

/** Tolerate a model that wraps JSON in a ```json fence or adds prose around it. */
function extractJson(text: string): string {
  const fenced = text.match(/```(?:json)?\s*([\s\S]*?)```/i);
  if (fenced) return fenced[1].trim();
  const start = text.indexOf("{");
  const end = text.lastIndexOf("}");
  if (start !== -1 && end > start) return text.slice(start, end + 1);
  return text.trim();
}

export async function agent(
  input: { description: string },
  chidori: Chidori,
) {
  // 1. The model decides the *content* of the screen, within the UiSpec contract.
  const answer = await chidori.prompt(
    "You are generating a UI. Return ONLY a JSON object matching this TypeScript " +
      "type, no prose:\n" +
      "{ title: string; subtitle?: string; features?: string[]; " +
      "badges?: { label: string; value: string }[]; cta?: string }\n\n" +
      "Build a screen for: " +
      input.description,
    { type: "ui-spec" },
  );
  const spec = JSON.parse(extractJson(answer)) as UiSpec;

  // 2. Trusted, deterministic render → mount into the journaled DOM.
  const markup = renderToStaticMarkup(React.createElement(Screen, { spec }));
  let root = document.getElementById("root");
  if (!root) {
    root = document.createElement("div");
    root.id = "root";
    document.body.appendChild(root);
  }
  root.innerHTML = markup;

  // 3. Flush the mutation batch as a durable `dom_render` effect.
  const batch = chidori.renderDOM();

  return {
    spec,
    html: root.innerHTML,
    mutations: batch.mutations.length,
    version: batch.version,
  };
}
