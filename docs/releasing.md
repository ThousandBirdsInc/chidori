# Releasing

Chidori ships from a single version train: the root `chidori` crate, the
TypeScript SDK, and the Python SDK all carry the same `X.Y.Z` version, and a
release is cut by pushing a `vX.Y.Z` tag.

| Artifact | Registry | Name | How it publishes |
| --- | --- | --- | --- |
| Rust crates | crates.io | `chidori-js`, `chidori` | `.github/workflows/release.yml` on tag push (or manually via `./scripts/publish.sh`) |
| TypeScript SDK | npm | [`@1kbirds/chidori`](https://www.npmjs.com/package/@1kbirds/chidori) | `.github/workflows/release.yml` on tag push |
| Python SDK | PyPI | [`chidori`](https://pypi.org/project/chidori/) | `.github/workflows/release.yml` on tag push |
| GitHub release | GitHub | tag `vX.Y.Z` | `.github/workflows/release.yml` on tag push (auto-generated notes) |

The unscoped npm name `chidori` belongs to an unrelated project, so the
TypeScript SDK publishes under the `@1kbirds` scope (which also carries the
legacy 0.1.x SDK releases). Agent authors who want the historical
`import ... from "chidori"` spelling can install via an npm alias:

```bash
npm install chidori@npm:@1kbirds/chidori
```

The runtime accepts both `"chidori"` and `"@1kbirds/chidori"` as the virtual
SDK module specifier in agent files.

## Cutting a release

1. Bump the version in all three places:
   - `crates/chidori/Cargo.toml` (`[package] version`)
   - `sdk/typescript/package.json` (then `npm install --package-lock-only`
     in `sdk/typescript` to refresh the lock file)
   - `sdk/python/pyproject.toml` (`[project] version`)
2. Sanity-check the train locally:

   ```bash
   ./scripts/check-sdk-versions.sh X.Y.Z
   ```

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

The npm and PyPI jobs authenticate with OIDC trusted publishing — no
long-lived tokens. The crates.io job uses a `CARGO_REGISTRY_TOKEN` secret. Each
needs a one-time configuration by an owner of the package:

**npm (`@1kbirds/chidori`)** — on npmjs.com, package → Settings → Trusted
publisher → GitHub Actions, with:

- Organization or user: `ThousandBirdsInc`
- Repository: `chidori`
- Workflow filename: `release.yml`
- Environment: `npm`

**PyPI (`chidori`)** — on pypi.org, project → Manage → Publishing → Add a new
publisher → GitHub, with:

- Owner: `ThousandBirdsInc`
- Repository name: `chidori`
- Workflow name: `release.yml`
- Environment name: `pypi`

**crates.io (`chidori` and `chidori-js`)** — create a crates.io API token at
<https://crates.io/settings/tokens> (scoped to publish-update, and ideally
limited to these two crates), then store it as a secret named
`CARGO_REGISTRY_TOKEN`. Put it on the `crates-io` environment (repository
Settings → Environments → `crates-io` → Secrets) so it's only exposed to the
`crates` job, or as a repository secret if you prefer.

**GitHub** — create the `npm`, `pypi`, and `crates-io` environments under
repository Settings → Environments. The `npm` and `pypi` ones can be empty; the
`crates-io` one carries the `CARGO_REGISTRY_TOKEN` secret. Add required
reviewers to any of them if publishes should need manual approval.

Until the OIDC publishers and the crates.io token are configured, tag pushes
will fail in the publish steps with an authentication error; the verify step
still runs.

The GitHub release step needs no setup — it uses the workflow's built-in
`GITHUB_TOKEN` with `contents: write`.
