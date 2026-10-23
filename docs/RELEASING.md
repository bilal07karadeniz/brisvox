# Releasing

How a release of brisvox is made: what it consists of, the steps a maintainer takes, what each
workflow does, and the registry settings that are entered once. Everything is released together
under one version number.

## What a release contains

| Artifact | Where | Name | Built and published by |
|---|---|---|---|
| Prebuilt `brisvox-server` | GitHub Release assets | `brisvox-server-vX.Y.Z-<target>.tar.gz` (`.zip` on Windows), `SHA256SUMS` | `release.yml`: `binaries`, `github-release` |
| Container image | `ghcr.io/bilal07karadeniz/brisvox-server` | tags `vX.Y.Z` and `latest`, `linux/amd64` + `linux/arm64` | `release.yml`: `docker-build`, `docker-push`, `docker-manifest` |
| Rust crates | crates.io | eleven crates, see below; `brisvox` is the umbrella | `release.yml`: `crates-dry-run`, `publish-crates` |
| Python SDK | PyPI | `brisvox-agent`, then the umbrella `brisvox` | `release.yml`: `python-dist`, `publish-pypi` |
| TypeScript SDK | npm | `@brisvox/agent` | `release.yml`: `npm-pack`, `publish-npm` |
| Browser module | npm | `@brisvox/web` | `release.yml`: `npm-pack`, `publish-npm` |
| npm umbrella | npm | `brisvox` (after the two above) | `release.yml`: `npm-umbrella`, `publish-npm-umbrella` |
| The book | GitHub Pages | https://bilal07karadeniz.github.io/brisvox/ | `pages.yml` (on every push to `main`) |

Prebuilt targets: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin`
and `x86_64-pc-windows-msvc`. The Linux binaries are built on Ubuntu 24.04 and need glibc 2.39 or
newer (the prebuilt ONNX Runtime needs 2.38). The binaries and the image are built with the feature
set `all-providers,call-control,smart-turn-ort,mcp,webrtc` (every provider adapter, call control, Smart
Turn on ONNX Runtime, MCP tools, the WebRTC listener); the README says so under "What to build with". The
Smart Turn model is not inside the binary archives (its licence notice and a checksum-verified
downloader are); the container image contains it.

### The umbrella packages

The plain name `brisvox` is a real package on all three registries, so `cargo add brisvox`,
`pip install brisvox` and `npm i brisvox` work next to the specific names. They hold no logic of
their own and are released with everything else, under the same version:

| Registry | Package | Source | What it is |
|---|---|---|---|
| crates.io | `brisvox` | `crates/brisvox` | Re-exports `brisvox-server` (`brisvox::server`), the tools API (`brisvox::agent`), the provider adapters (`brisvox::providers`) and the core types (`brisvox::core`); `brisvox::run_with_tools` is a whole `main`. Forwards the features of `brisvox-server` by the same names (default: `yaml`, `silero-tract`; not the binary's `mimalloc`). Published last of the crates. |
| PyPI | `brisvox` | `sdk/python-umbrella` | Depends on `brisvox-agent==X.Y.Z` and re-exports its public API (`from brisvox import Agent`). |
| npm | `brisvox` | `sdk/npm-umbrella` | Depends on `@brisvox/agent` and `@brisvox/web` at exactly `X.Y.Z`; `brisvox` re-exports the agent SDK (Node), `brisvox/web` the browser client. |

`scripts/check_release.py` keeps them honest: one version, the exact pins, every feature of
`brisvox-server` forwarded, the license files in place.

### The crates and their publish order

crates.io needs every dependency of a crate to exist before the crate itself, so the crates are
published in this order (`python3 scripts/check_release.py --order` prints it and
`scripts/check_release.py` fails when the order stops being a valid dependency order):

1. `brisvox-core`
2. `brisvox-audio`
3. `brisvox-inference`
4. `brisvox-agent`
5. `brisvox-providers`
6. `brisvox-sim`
7. `brisvox-transport`
8. `brisvox-engine`
9. `brisvox-agent-link`
10. `brisvox-server`
11. `brisvox` (the umbrella: it depends on `brisvox-agent`, `brisvox-core`, `brisvox-providers` and
    `brisvox-server`, so it goes last)

`brisvox-bench` (the benchmark harness) and everything under `examples/` are never published
(`publish = false`). `brisvox-sim` is published because `brisvox-engine` has an optional dependency
on it (its `sim` feature); it is also the mock providers and simulator for testing an agent. Two
rules in the manifests keep publishing possible: every dependency between workspace crates carries
`version` next to `path`, and every *dev*-dependency between workspace crates is path-only (no
version), so cargo drops it from the published manifest. Several crates are each other's
dev-dependency (`brisvox-agent` and `brisvox-sim`), which would otherwise be a cycle.

Each package contains `src/`, its README and the two license files, nothing else (`include` in the
manifest): no tests, fixtures, examples or benchmark output. `cargo publish` prints "ignoring test
... is not included in the published package" for every integration test; that is expected. The
tests run from the repository. The Silero VAD graph that `brisvox-inference` embeds (MIT) is in the
package under `assets/` with its NOTICE.

## Making a release

1. **Bump the version** in one commit: `version` in the workspace `Cargo.toml` and the
   `version = "..."` of the eleven `brisvox*` entries in its `[workspace.dependencies]`;
   `sdk/python/pyproject.toml` and `__version__` in `sdk/python/brisvox_agent/__init__.py`;
   `sdk/python-umbrella/pyproject.toml` (its `version` and the `brisvox-agent==X.Y.Z` pin) and
   `__version__` in `sdk/python-umbrella/brisvox/__init__.py`; and in `sdk/typescript` and
   `sdk/web` run `npm version X.Y.Z --no-git-tag-version` (it also updates the lock file). In
   `sdk/npm-umbrella` run the same and set both entries under `dependencies`
   (`@brisvox/agent`, `@brisvox/web`) to `X.Y.Z`. Then `cargo check` so `Cargo.lock` follows.
2. **Write the changelog.** In `CHANGELOG.md` move the entries of `[Unreleased]` under a new
   `## [X.Y.Z] - YYYY-MM-DD` heading and update the two compare links at the bottom. The text of
   that section becomes the GitHub Release notes.
3. **Check locally.**
   ```sh
   python3 scripts/check_release.py        # versions, metadata, licenses, publish order, workflows
   scripts/publish_crates.sh --dry-run     # package and build all eleven crates, no token
   scripts/verify.sh                       # the full quality gate
   ```
4. **Merge to `main`** and wait for the `ci` run on that commit to finish green. The release
   workflow refuses a tag whose commit has no successful `ci` run.
5. **Rehearse (optional, recommended for the first release and after changing a workflow).**
   Actions > release > Run workflow on `main`. It does everything up to the gate (all binaries,
   both image platforms, the Python and npm packages, the crate dry run) and publishes nothing.
6. **Tag and push the tag.**
   ```sh
   git tag -a vX.Y.Z -m "brisvox X.Y.Z"
   git push origin vX.Y.Z
   ```
   A version with a hyphen (`0.2.0-rc.1`) is a pre-release: the GitHub Release is marked as one and
   the image does not get the `latest` tag.
7. **Watch the run.** If one publish job fails (a registry hiccup, the crates.io rate limit), fix the
   cause and re-run that job from the Actions page. Publishing is idempotent: crates, PyPI files,
   npm versions and image tags that already exist are skipped or replaced with the same content.

A published registry version cannot be replaced. A broken release is fixed by publishing the next
patch version; `cargo yank`, `npm deprecate` and the PyPI "yank" mark the broken one.

## What the workflows do

### `release.yml` (on a `v*` tag; Run workflow = rehearsal)

Build stage, nothing is published here:

- `prepare`: `scripts/check_release.py --tag` (the tag names the workspace version; crates, Python
  and npm packages carry the same version; the changelog has the entry; metadata, license files and
  publish order are right), extracts the release notes, and requires a successful `ci` run on the
  tagged commit.
- `crates-dry-run`: `cargo publish --workspace --dry-run`, which packages all eleven crates in
  dependency order and builds each from its package against the ones before it.
- `python-dist`: runs the SDK tests, builds the sdist and wheel of `brisvox-agent` and of the
  umbrella `brisvox`, `twine check --strict` on both, and runs the umbrella's tests with both wheels
  installed (the umbrella's pin on `brisvox-agent` must resolve to the wheel of this release).
- `npm-pack`: tests and `npm pack` of `sdk/typescript` and `sdk/web`.
- `npm-umbrella`: the static checks of `sdk/npm-umbrella`, `npm pack` of it, and a fresh install of
  its tarball together with the two tarballs of `npm-pack`, importing `brisvox` and `brisvox/web`.
- `binaries`: `cargo build --release --locked` on a native runner per target, strip, a smoke test
  (`--version`, `check` of a reference definition), and `scripts/package_release.py` (archive plus
  `.sha256`).
- `docker-build`: the Dockerfile for `linux/amd64` and `linux/arm64`, each on a native runner,
  without pushing.
- `gate`: succeeds only when every job above did.

Publish stage, tags only, each job needs `gate`:

- `github-release`: creates the GitHub Release for the tag with the changelog section as notes,
  the archives and `SHA256SUMS`.
- `docker-push` and `docker-manifest`: build each platform again (from the build cache), push it
  by digest to GHCR with the repository's `GITHUB_TOKEN`, then create one multi-platform image with
  the tags `vX.Y.Z` and `latest`.
- `publish-crates`: `scripts/publish_crates.sh`, which publishes the crates in the order above
  (the umbrella `brisvox` last), skips versions that already exist and waits out crates.io's limit
  on new crates.
- `publish-pypi`: `pypa/gh-action-pypi-publish` with trusted publishing, environment `pypi`:
  `brisvox-agent`.
- `publish-pypi-umbrella`: the same for `brisvox`, environment `pypi-brisvox`, after
  `publish-pypi` (PyPI allows one pending publisher per repository + workflow + environment).
- `publish-npm`: `npm publish --provenance --access public` of the tarballs made by `npm-pack`
  (`@brisvox/agent`, `@brisvox/web`).
- `publish-npm-umbrella`: the same for `brisvox`, only after `publish-npm` succeeded (it depends on
  the two scoped packages being on the registry).

### `pages.yml` (on every push to `main`)

Checks the book structure (`scripts/check_book.py`), builds it with mdBook (`mdbook build book`) and
deploys it to GitHub Pages with `actions/deploy-pages`.

## One-time setup

Nothing here is needed again for later releases, except that the first release of each name needs a
token (below), and the token is removed afterwards.

### Repository settings (GitHub)

- Settings > Pages > Build and deployment > Source: **GitHub Actions**.
- Settings > Code security > **Private vulnerability reporting**: enabled (`SECURITY.md` points
  reporters there).
- Settings > Actions > General > Workflow permissions: **Read repository contents** is enough; the
  workflows declare what they need.
- Settings > Environments: create `pypi` (the workflow creates it on first use; add required
  reviewers if every PyPI upload should wait for a person).
- After the first image push: the package `brisvox-server` starts private. On the repository's
  Packages page open it, Package settings > Danger Zone > **Change visibility: Public**, and
  confirm that the repository is linked (the image carries the `org.opencontainers.image.source`
  label).

### crates.io

Trusted publishing is configured per crate and, as of 2026-10-15, only for a crate that already
exists; the crates.io announcement says the first release of a crate is published with a token.

0. Verify the account's email address at https://crates.io/settings/profile: crates.io refuses
   every publish from an account without a verified email (`400 Bad Request: A verified email
   address is required`), token or not.
1. Create a token at https://crates.io/settings/tokens: scopes **publish-new** and
   **publish-update**, crate patterns `brisvox-*` **and `brisvox`** (the pattern `brisvox-*` does
   not match the umbrella crate's plain name), an expiry of a few weeks.
2. Add it as the repository secret **`CARGO_REGISTRY_TOKEN`** (Settings > Secrets and variables >
   Actions). While this secret exists the workflow uses it instead of OIDC.
3. Push the first tag. `publish-crates` publishes all eleven crates; crates.io lets you publish five
   new crates at once and then one every ten minutes, so the job takes about an hour and waits by
   itself. (To avoid the wait, write to help@crates.io before the first release for a higher limit.)
4. For **each of the eleven crates** (including `brisvox`) open its settings page on crates.io (for the first one the address
   ends in `/crates/brisvox-core/settings`) > Trusted Publishing > Add, choose GitHub and enter:

   | Field | Value |
   |---|---|
   | Repository owner | `bilal07karadeniz` |
   | Repository name | `brisvox` |
   | Workflow filename | `release.yml` |
   | Environment | leave empty |

5. Delete the secret `CARGO_REGISTRY_TOKEN` and revoke the token. From the next release the job
   authenticates with `rust-lang/crates-io-auth-action` (OIDC, a short-lived token). Optionally turn
   on "Require trusted publishing for new versions" in each crate's settings.

### PyPI

PyPI can register a publisher before the project exists (a *pending publisher*), so no token is
ever needed. There are **two** projects, so add **two** pending publishers at
https://pypi.org/manage/account/publishing/ > "Add a new pending publisher" > GitHub, one for each
PyPI project name (`brisvox-agent` and the umbrella `brisvox`). PyPI allows only one pending
publisher per repository + workflow + environment, so each uses its own environment:

| Field | Value |
|---|---|
| PyPI project name | `brisvox-agent` for the first, `brisvox` for the second |
| Owner | `bilal07karadeniz` |
| Repository name | `brisvox` |
| Workflow name | `release.yml` |
| Environment name | `pypi` for `brisvox-agent`, `pypi-brisvox` for `brisvox` |

The first successful publish creates each project and turns its pending publisher into an
ordinary trusted publisher. A pending publisher does not reserve the name: if somebody registers
`brisvox-agent` or `brisvox` first, it stops working, so publish soon after adding them. The
workflow publishes `brisvox-agent` first and `brisvox` second, in two jobs; if only one pending
publisher exists, the other upload fails with a trusted-publishing error and the job can be
re-run after adding it (`skip-existing` makes the finished one a no-op).

### npm

An npm trusted publisher can only be added to a package that already exists, so the first version of
each package is published with a token. The scope needs an owner. There are three packages:
`@brisvox/agent`, `@brisvox/web` and the unscoped `brisvox` (the umbrella, a name on the public
namespace rather than in the `@brisvox` scope).

1. Create the organization **`brisvox`** on https://www.npmjs.com/org/create (free for public
   packages); that owns the `@brisvox` scope.
2. Create a granular access token (https://www.npmjs.com/settings/~/tokens): read and write,
   allowed to bypass two-factor authentication (CI has no second factor), a short expiry. Its
   scope must cover **both** the `@brisvox` scope **and the unscoped package `brisvox`**: a token
   limited to `@brisvox` cannot publish `brisvox`, which is outside the scope. A granular token
   can only name packages that already exist, and `brisvox` does not yet, so for the first
   publish give it access to **all packages** (read and write) with a short expiry, and revoke it
   in step 5. Add it as the repository secret **`NPM_TOKEN`**.
3. Push the first tag. `publish-npm` publishes `@brisvox/agent` and `@brisvox/web` with
   `--provenance --access public`, then `publish-npm-umbrella` publishes `brisvox` the same way.
4. For **each of the three packages** (`@brisvox/agent`, `@brisvox/web`, `brisvox`) open
   npmjs.com > the package > Settings > Trusted Publisher >
   GitHub Actions and enter (case-sensitive; npm does not validate them when you save, a mistake
   shows at publish time):

   | Field | Value |
   |---|---|
   | Organization or user | `bilal07karadeniz` |
   | Repository | `brisvox` |
   | Workflow filename | `release.yml` |
   | Environment name | leave empty |

   With npm 11.15 or newer and two-factor authentication on the account, the same can be done with
   `npm trust github @brisvox/agent --repo bilal07karadeniz/brisvox --file release.yml --allow-publish`
   (and the same for `@brisvox/web` and `brisvox`).
5. Delete the secret `NPM_TOKEN` and revoke the token; under each of the three packages' Publishing access choose
   "Require two-factor authentication and disallow tokens". From the next release npm authenticates
   by OIDC (npm 11.5.1 and Node 22.14 or newer, which the job has). The package's `repository.url`
   must match this repository exactly; `package.json` has it.

## Registry facts

The registry behaviour described here (trusted publishing on crates.io, PyPI and npm; the first
release of a name; the crates.io rate limit on new crates; the container registry's default
visibility) was read from the registries' own documentation and sources in October 2026. Registries
change: when a step stops matching what the website shows, the website is right and this page
should be corrected.
