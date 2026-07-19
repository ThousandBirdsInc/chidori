---
title: "Prompt Templates"
---

# Templates — Jinja prompt rendering

> `chidori.template` renders Jinja2-syntax templates (inline strings or
> `.jinja`/`.j2` files) with [minijinja], as a recorded durable host call.
> **Related:** `docs/core-concepts.md`, `docs/replay.md`. API reference:
> `llm.txt`. Implementation: `crates/chidori/src/runtime/template.rs`.

[minijinja]: https://docs.rs/minijinja/

## 1. What this is

Prompt text wants to be data, not string concatenation. `chidori.template`
takes a template — inline or from a file — plus a JSON object of variables,
and returns the rendered string:

```ts
// Inline template string.
const greeting = await chidori.template("Hello {{ name }}!", { name: "world" });

// File template, resolved relative to the agent's directory.
const prompt = await chidori.template("prompts/summary.jinja", {
  document: input.document,
});

const summary = await chidori.prompt(prompt, { type: "final" });
```

Full Jinja2 syntax is supported: `{{ var }}` interpolation, `{% if %}` /
`{% for %}` blocks, filters, and `{% include %}` / `{% extends %}` composition
for file templates.

## 2. Inline vs. file templates

The first argument is interpreted by suffix: a string ending in `.jinja` or
`.j2` is treated as a **file path**; anything else is rendered as an **inline
template string**. There is no separate option to force one or the other —
name your template files with one of those two extensions.

## 3. How file paths resolve

File template paths resolve relative to the **project base directory — the
agent file's directory** (the same root that anchors `chidori.callAgent`
sub-agent paths). This holds across `run`, `resume`, `serve`, and the branch
commands, so `prompts/summary.jinja` means the same file no matter which
directory you launch `chidori` from.

`{% include %}` and `{% extends %}` inside a file template resolve relative to
**that template file's own directory**, so a template tree can live in its own
folder (`prompts/base.jinja`, `prompts/partials/header.jinja`) and reference
its siblings with local names. A referenced template that does not exist fails
the render with a template-not-found error.

## 4. Undefined variables fail loudly

The engine runs minijinja with `UndefinedBehavior::SemiStrict`. In practice:

- **Printing an undefined variable is an error.** `Hello {{ name }}!` with no
  `name` in the variables fails the call — a typo cannot silently render as an
  empty string and flow into a prompt.
- **Iterating, attribute access, and filter coercion of undefined also fail.**
  `{% for item in items %}` with a missing (or non-iterable) `items` errors,
  as does `{{ user.name }}` when `user` is undefined.
- **Truthiness checks are allowed.** `{% if verbose %}…{% endif %}` treats an
  undefined `verbose` as false, so *optional* variables are expressed with an
  `{% if %}` guard rather than by relying on empty-string rendering.

A failed render rejects the `chidori.template` promise with the minijinja
error (naming the template and the failing operation).

Whitespace control: `trim_blocks` and `lstrip_blocks` are enabled, so block
tags (`{% … %}`) do not leak their trailing newline or leading indentation
into the output — templates can be indented for readability without producing
ragged prompt text.

## 5. Durability and replay

Every `chidori.template` call is a recorded `template` host call in the run's
journal: the rendered string is journaled live, and **replay returns the
recorded output without re-reading the template file**. Consequences:

- Replays (`chidori resume`, `chidori verify`, server replay) are stable even
  if a template file has been edited or deleted since the run was recorded.
- Conversely, editing a template does not change what an existing checkpoint
  replays — re-run the agent to render with the new template.

Template rendering is a *pure* effect — it is never policy-gated, so it works
identically under `--trusted`, ask-mode, and `untrusted` profiles.

## 6. When to reach for it

| Need | Use |
|---|---|
| Reusable prompt text with variables, conditionals, loops | `chidori.template` |
| Multi-turn structure, shared cacheable prefixes | `chidori.context()` ([context management](./context-management.md)) |
| One-off short prompt | A template literal is fine |

The two compose: render a template into a string, then feed it to
`chidori.prompt`, a `context()` turn, or a `conversation()` system prompt.
