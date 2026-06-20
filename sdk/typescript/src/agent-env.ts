/**
 * Ambient types for the virtual `chidori:agent` module.
 *
 * Agent and tool files written for the Chidori runtime import their authoring
 * types — and, in the global style, the `chidori`/`run` values — from
 * `chidori:agent`:
 *
 * ```ts
 * import type { Chidori, ToolDefinition } from "chidori:agent";
 * // …or the global style:
 * import { chidori, run } from "chidori:agent";
 * ```
 *
 * There is no installable package behind that specifier. It is a virtual module
 * the runtime injects, much like `node:fs` or `bun:test`: the runtime strips the
 * import and supplies the real values when it executes the file, so nothing is
 * resolved from npm. Crucially, the unrelated `chidori` package on npm can never
 * be pulled in by mistake, because `chidori:agent` is not a registry name at all.
 *
 * This file exists only so editors and `tsc` can type agent files. Pull it into
 * a project with a triple-slash reference at the top of an agent or tool file:
 *
 * ```ts
 * /// <reference types="@1kbirds/chidori/agent-env" />
 * import type { Chidori } from "chidori:agent";
 * ```
 *
 * or once, project-wide, via tsconfig `compilerOptions.types`:
 * `["@1kbirds/chidori/agent-env"]`.
 *
 * NOTE: this must stay a script file (no top-level `import`/`export`). A bare
 * `declare module "chidori:agent"` is a *global* ambient declaration; adding a
 * top-level `export` would turn it into a module augmentation and fail to
 * resolve, since there is no real `chidori:agent` module to augment.
 */
declare module "chidori:agent" {
  // Re-export every authoring type plus the `chidori`/`run` value globals from
  // the SDK's agent module. Ambient module declarations may only reference
  // other modules by a non-relative (package) name, so we use the package's own
  // public subpath. The package's build resolves it via the `paths` mapping in
  // tsconfig.json; consumers resolve it from `node_modules`.
  export * from "@1kbirds/chidori/agent";
}
