# Releasing

Chidori ships from a single version train: the root `chidori` crate, the
TypeScript SDK, and the Python SDK all carry the same `X.Y.Z` version, and a
release is cut by pushing a `vX.Y.Z` tag.

| Artifact | Registry | Name | How it publishes |
| --- | --- | --- | --- |
| Rust crates | crates.io | `chidori-js`, `chidori` | `.github/workflows/release.yml` on tag push (or manually via `./scripts/publish.sh`) |
| TypeScript SDK | npm | [`@1kbirds/chidori`](https://www.npmjs.com/package/@1kbirds/chidori) | `.github/workflows/release.yml` on tag push |
| Python SDK | PyPI | [`chidori`](https://pypi.org/project/chidori/) | `.github/workflows/release.yml` on tag push |
| Prebuilt binaries | GitHub release assets | `chidori-vX.Y.Z-<target>.tar.gz` (each with a `.sha256`) | `.github/workflows/release.yml` `binaries` job on tag push |
| GitHub release | GitHub | tag `vX.Y.Z` | `.github/workflows/release.yml` on tag push (auto-generated notes, with the prebuilt binaries attached) |

The prebuilt binaries are what `scripts/install.sh` (the `curl | sh` quickstart)
downloads, so users get the `chidori` runtime without a Rust toolchain. The
`binaries` job builds one natively per target — `aarch64`/`x86_64-apple-darwin`
and `x86_64`/`aarch64-unknown-linux-gnu` — and the `github-release` job attaches
them to the release. The binary links rustls (not OpenSSL/native-tls), so each
tarball is self-contained; Linux builds on the oldest supported runner for a low
glibc floor. Windows isn't prebuilt — those users install with `cargo install
chidori`. To add or drop a target, edit the `binaries` matrix and keep the
asset-name pattern in `scripts/install.sh` in sync.

The unscoped npm name `chidori` belongs to an unrelated project, so the
TypeScript SDK publishes under the `@1kbirds` scope (which also carries the
legacy 0.1.x SDK releases). The two import contexts use different specifiers,
and neither can pull the unrelated package:

- **Agent and tool files** (executed by the runtime) import their authoring
  types from the virtual module `"chidori:agent"`. It is a URL-style scheme —
  like `node:fs` — with no registry name behind it, so it can never be
  `npm install`ed by mistake. The runtime strips the import and injects the
  values at execution time; editor types ship as an ambient declaration in
  `@1kbirds/chidori` (pulled in with
  `/// <reference types="@1kbirds/chidori/agent-env" />`). The runtime accepts
  only `"chidori:agent"`; the old `"chidori"` / `"@1kbirds/chidori"` spellings
  now fail with a migration error.
- **Client/driver code** (a normal Node program talking to `chidori serve`)
  imports the HTTP client from the real package, `"@1kbirds/chidori"`:

  ```bash
  npm install @1kbirds/chidori
  ```

## Cutting a release

1. Bump the version in all three places:
   - `crates/chidori/Cargo.toml` (`[package] version`)
   - `sdk/typescript/package.json` (then `npm install --package-lock-only`
     in `sdk/typescript` to refresh the lock file)
   - `sdk/python/pyproject.toml` (`[project] version`)
2. Sanity-check the train locally:

   ```bash
   ./scripts/check-sdk-versions.sh X.Y.Z
   ./scripts/check-npm-drift.sh
   ```

   The second script guards the skip-if-published behavior below: because a
   version already on npm is never re-published, **any change to
   `sdk/typescript` that ships to users requires a version bump** — otherwise
   the npm package silently stays stale at the old contents (this is how the
   published 3.6.0 ended up missing the 3.6.0 runtime's `defineTool` types).
   The script fails when the tree drifts from the published tarball of the
   same version; CI runs it on every PR and the release workflow re-checks it
   before deciding to skip.

3. Commit, merge to `main`, then tag and push:

   ```bash
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```

4. The `Release` workflow verifies the tag matches the version train, then
   publishes the whole train — the `chidori-js` and `chidori` crates to
   crates.io, the TypeScript SDK to npm, and the Python SDK to PyPI — skipping
   any version already on its registry, and finally creates the matching
   GitHub release with auto-generated notes. Re-running the workflow on the
   same tag is safe: each publish step is a no-op once that version exists, and
   the GitHub release step skips if the release is already there.

To rehearse without publishing, run the workflow manually from the Actions
tab (`workflow_dispatch`): it runs the crates `cargo publish --dry-run`, `npm
publish --dry-run`, and `twine check`, uploads nothing, and creates no GitHub
release.

The `crates` job reuses `./scripts/publish.sh`, so you can still publish the
crates by hand from a tagged commit if CI is unavailable:
`CARGO_REGISTRY_TOKEN=... ./scripts/publish.sh`.

## One-time registry setup

Create the three environments under repository Settings → Environments → New
environment (`npm`, `pypi`, `crates-io`), then configure each registry — done
by an owner of the package. All three authenticate with OIDC trusted
publishing, so no long-lived registry tokens are stored.

**npm (`@1kbirds/chidori`)** → environment `npm`, OIDC trusted publishing (no
secret). On npmjs.com, package → Settings → Trusted Publisher → GitHub Actions,
with:

- Organization or user: `ThousandBirdsInc`
- Repository: `chidori`
- Workflow filename: `release.yml`
- Environment: `npm`

**PyPI (`chidori`)** → environment `pypi`, OIDC trusted publishing (no secret).
On pypi.org, project → Manage → Publishing → Add a new publisher → GitHub, with:

- Owner: `ThousandBirdsInc`
- Repository name: `chidori`
- Workflow name: `release.yml`
- Environment name: `pypi`

**crates.io (`chidori` and `chidori-js`)** → environment `crates-io`, OIDC
trusted publishing (no secret). On crates.io, for *each* crate go to Settings →
Trusted Publishing → Add, with:

- Repository owner: `ThousandBirdsInc`
- Repository name: `chidori`
- Workflow filename: `release.yml`
- Environment: `crates-io`

Add required reviewers to any environment if a publish should need manual
approval. Until the npm/PyPI/crates.io trusted publishers are configured, tag
pushes fail in that registry's publish step with an authentication error; the
verify step still runs.

The GitHub release step needs no setup — it uses the workflow's built-in
`GITHUB_TOKEN` with `contents: write`.
