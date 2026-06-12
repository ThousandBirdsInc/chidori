# Releasing

Chidori ships from a single version train: the root `chidori` crate, the
TypeScript SDK, and the Python SDK all carry the same `X.Y.Z` version, and a
release is cut by pushing a `vX.Y.Z` tag.

| Artifact | Registry | Name | How it publishes |
| --- | --- | --- | --- |
| Rust crates | crates.io | `chidori`, `chidori-js` | Manually, via `./publish.sh` (see that script's header) |
| TypeScript SDK | npm | [`@1kbirds/chidori`](https://www.npmjs.com/package/@1kbirds/chidori) | `.github/workflows/release.yml` on tag push |
| Python SDK | PyPI | [`chidori`](https://pypi.org/project/chidori/) | `.github/workflows/release.yml` on tag push |

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
   - `Cargo.toml` (`[package] version`)
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

4. The `Release SDKs` workflow verifies the tag matches the version train,
   builds both SDKs, and publishes each one — skipping any version that is
   already on its registry, so re-running the workflow is safe.
5. Publish the crates separately: `CARGO_REGISTRY_TOKEN=... ./publish.sh`
   from the tagged commit.

To rehearse without publishing, run the workflow manually from the Actions
tab (`workflow_dispatch`): it builds both packages, runs `npm publish
--dry-run` and `twine check`, and uploads nothing.

## One-time registry setup

Both publish jobs authenticate with OIDC trusted publishing — no long-lived
tokens stored in repository secrets. Each registry needs a one-time trusted
publisher configuration by an owner of the package:

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

**GitHub** — create the `npm` and `pypi` environments under repository
Settings → Environments (they can be empty; add required reviewers if
publishes should need manual approval).

Until the trusted publishers are configured, tag pushes will fail in the
publish steps with an authentication error; the build/verify steps still run.
