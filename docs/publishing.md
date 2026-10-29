# Publishing the Polaris coding agent

The release contains four registry crates. `polaris_code` is the application
library and ships the `polaris` binary. Its supporting crates remain
independently reusable:

| Package | Role | Workspace dependencies |
| --- | --- | --- |
| `polaris-tui` | Terminal UI | None |
| `polaris_cosmos` | Cognitive models and composition contracts | None |
| `polaris_ontrace` | Knowledge graph capabilities | `polaris-tui` |
| `polaris_code` | Coding-agent application library and CLI | All three above |

Every crate shares the workspace's registry `polaris-ai` dependency.
`polaris-tui-examples` stays unpublished and is excluded from release commands.
This guide describes preparation and live release separately; a passing dry
run does not mean any version has been uploaded or any name reserved.

## Prepare the release

Use Rust/Cargo 1.97.0, which supports workspace packaging and publishing.
Start from a clean, reviewed release commit. Choose available package versions,
match each workspace path dependency's `version` requirement to its package,
and commit the resulting `Cargo.lock`.

Confirm the intended crates.io account can publish every package: check name
availability for new crates and ownership or team permissions for existing
ones. Repository access does not grant registry ownership. Authenticate using
Cargo's supported credential mechanism and meet the registry's current account
requirements. This guide does not assert that any of these names or versions
has already been published.

Run the workspace checks, including doctests and the embedding consumer tests:

```bash
cargo +1.97.0 check --workspace --locked
cargo +1.97.0 test --workspace --locked
cargo +1.97.0 fmt --all -- --check
cargo +1.97.0 clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo +1.97.0 doc --workspace --exclude polaris-tui-examples --no-deps --locked
```

Review the generated `polaris_code` API documentation and its embedding guide.
Verify the examples use the registry package name and declared public APIs,
and that required supporting dependencies are shown explicitly.

Package and verify the archives, then exercise the publication dry run:

```bash
cargo +1.97.0 package --workspace --exclude polaris-tui-examples --locked --list
cargo +1.97.0 package --workspace --exclude polaris-tui-examples --locked
cargo +1.97.0 publish --workspace --exclude polaris-tui-examples --locked --dry-run
```

Inspect the four `.crate` archives under `target/package/`. Check normalized
manifests, versioned registry dependencies, README and license files, embedded
API docs, and every source asset needed to build outside this repository.
Review archive contents for local artifacts and credentials. Keep verification
enabled: building only the workspace checkout does not prove the archives work.

After these checks pass, create an annotated tag at the reviewed commit. For
example, for application version 0.1.0:

```bash
git tag -a v0.1.0 -m "Release 0.1.0"
```

The tag records the tested source; creating it does not upload packages.

## Live release

The following command performs actual registry uploads:

```bash
cargo +1.97.0 publish --workspace --exclude polaris-tui-examples --locked
```

Cargo can orchestrate dependency order for the workspace. When uploading
packages individually, publish `polaris-tui` and `polaris_cosmos` first, then
`polaris_ontrace`, and finally `polaris_code`. Each dependency version must be
available from the registry before publishing its consumers.

Verify all four registry versions and their docs.rs builds, check installation
of the `polaris` binary and the documented library embedding example, then
push the matching release tag, for example `git push origin v0.1.0`.

## Application-only releases

The application version is declared in `crates/polaris_code/Cargo.toml`.
Supporting packages keep their own versions; do not republish an unchanged
supporting version when only the executable changes. For an application patch:

```bash
cargo +1.97.0 publish -p polaris_code --locked --dry-run
cargo +1.97.0 publish -p polaris_code --locked
```

The executable is named `polaris` starting with application version 0.1.1.
The Rust library remains `polaris_code`, and recorded session and evidence
identifiers keep their existing names.

## CI and binary releases

The [test and build workflow](../.github/workflows/ci.yml) runs for pull
requests, pushes to `main`, `v*` tags, and manual dispatches. It pins Rust
1.97.0 and uses the committed lockfile. Formatting, workspace checking and
tests (including doctests) run alongside release builds; Linux also runs
Clippy across all workspace targets and features. Native dependencies for the
optional RocksDB backend are installed for that all-features check.
The macOS test runner supplies `python` and `python3` through a virtual
environment and uses a canonical temporary directory for workspace fixtures.

Documentation and branding changes run quick Python checks, skipping Rust
compilation, workspace tests, release builds, and Homebrew installation tests.
The full pipeline runs when crate sources, tests, examples, benches, embedded
documentation, ontology inputs, evaluation code or fixtures, Cargo dependencies,
toolchain settings, or CI/build/release tooling change.
Pushes compare the entire before/after range; pull requests
compare against their merge base. If the range cannot be compared, CI runs
the full pipeline. Release tags and manual dispatches always run it too.

The final `CI` check verifies every job required for that change; use it as
the required status check in branch protection. Documentation-only pushes do
not cancel source builds already in progress. A newer source build cancels
the older job for the same platform and branch; tagged jobs are retained.

The workspace consumes the published `polaris-ai` 0.7.0 dependency rather than
a developer's sibling checkout. The published 0.6.0 package lacks graph APIs
that this workspace already uses. Every workspace crate still inherits the
single root dependency.

The build matrix produces:

| Archive target | Minimum platform |
| --- | --- |
| `aarch64-apple-darwin` | Apple Silicon, macOS Sequoia 15 |
| `x86_64-unknown-linux-gnu` | Linux x86-64, glibc 2.35 (Ubuntu 22.04 baseline) |

Each `polaris-code-<version>-<target>.tar.gz` contains the executable, license,
README, and its logo assets, with a matching `.tar.gz.sha256` file. Each runner
extracts its archive and checks the version, help, and JSON topology inventory
without model credentials. The Linux archive uses glibc; continue using
`scripts/build-portable-binary.sh` for the static musl benchmark executable.

Download the archives from the workflow's `polaris-<target>` artifacts within
14 days. Only after all checks and both builds pass does the workflow generate
the `homebrew-formula` artifact. For a pushed tag in
`RadiantAILabs/polaris_code`, it also creates a **draft** GitHub release with
the same archives, checksums, and `polaris-code.rb`. Tags must match the
application version exactly (`v0.1.1` for application 0.1.1); supporting crate
versions do not determine this tag. This job does not publish registry crates.
Separate macOS and Linux runners install the generated formula from the cached
CI archives, run its functional tests, and check linkage before release
creation. This tests the actual Homebrew installation without requiring the
release URLs to be public during pull requests.
Review and publish the draft to make its download URLs publicly available.
If retrying release creation, remove the incomplete draft first; the workflow
does not overwrite an existing release.

Homebrew can reuse those exact binaries through the generated formula. It
selects the archive for the OS and CPU, verifies SHA-256, and installs
`polaris` without Rust or CMake. To regenerate it locally after downloading
both archives and their checksum files:

```bash
python3 scripts/homebrew_formula.py 0.1.1 RadiantAILabs/polaris_code dist
```

The generator requires Python 3.11 or newer and rejects missing platforms and
checksum mismatches. It writes `dist/polaris-code.rb`. Its URLs point to the
tagged GitHub release, so publish the release before installing from it. CI
artifacts expire and require authentication; they are an intermediate handoff,
not the formula's download URLs.

## Homebrew

The public [Radiant AI tap](https://github.com/RadiantAILabs/homebrew-tap)
maintains `Formula/polaris-code.rb`. Installation is:

```bash
brew install radiantailabs/tap/polaris-code
polaris --version
```

The formula name distinguishes this coding agent from Homebrew's unrelated
Kubernetes `polaris` package. Both install a `polaris` executable, so the tap
declares that conflict.

After pushing the reviewed source tag, publish a stable GitHub Release in
`RadiantAILabs/polaris_code` for `vMAJOR.MINOR.PATCH`. The Homebrew release
workflow dispatches that tag to the tap. Pushing a tag alone does not trigger
it; prereleases are excluded. If another workflow creates the GitHub Release,
use a GitHub App or personal access token, since releases created with the
repository's `GITHUB_TOKEN` do not trigger another workflow.

The tap checks that the source archive declares the matching `polaris_code`
version and `polaris` executable, computes SHA-256 from the downloaded bytes,
and stages the new formula on its runners. The live formula keeps its previous
bottles until both new builds pass. The formula fetches the locked Cargo
dependencies and then installs offline. Its functional test checks the installed
command's version, help, and a real JSON topology inventory without a model key.

The tap's GitHub Actions workflow builds and tests bottles on Apple Silicon
macOS Sequoia and Linux x86-64. It validates the archive checksums, publishes
them as immutable GitHub release assets, and merges Homebrew's bottle metadata
into the formula. Separate fresh runners then install with `--force-bottle`,
verify the installation receipt, and run the functional tests. The macOS job
also checks that Golden Gate selects the compatible Apple Silicon bottle.

Each batch uses its own download URL so previous bottle assets are preserved.
Bottles skip Rust and CMake build dependencies during installation; platforms
without a compatible bottle use the formula's existing source-build path.

### One-time automation setup

Create a fine-grained GitHub personal access token owned by `RadiantAILabs`,
limited to the `homebrew-tap` repository, with **Actions: write** permission
(and automatic Metadata: read). Complete organization approval if required.
Store it as the `HOMEBREW_TAP_TOKEN` repository Actions secret in
[`RadiantAILabs/polaris_code` settings](https://github.com/RadiantAILabs/polaris_code/settings/secrets/actions).
Do not commit it or reuse a broad development credential. The source repository's
`GITHUB_TOKEN` cannot dispatch workflows in the separate tap. The tap uses its
own job-scoped token to publish assets and push its formula; its repository
settings must allow Actions to write contents and push to `main`.

See GitHub's [workflow dispatch API permissions](https://docs.github.com/en/rest/actions/workflows#create-a-workflow-dispatch-event)
and [workflow event authentication rules](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/trigger-a-workflow).

### Retry and verify

Run the public repository's **Homebrew release** workflow manually with the
published `release_tag` to replay a notification. An already bottled version
is skipped; an older version cannot replace a newer formula. Enable
`force_rebuild` to rebuild the current version. After the workflows are installed,
a manual replay of the current release verifies authentication and release
validation without rebuilding it.

The tap's **Homebrew bottles** workflow also accepts `release_tag` and
`force_rebuild`, or `verify_only` to test existing bottles on fresh runners.
Use **Re-run all jobs** after a build or publication failure so both artifacts
share the new run attempt's immutable download URL. Builds are pinned to the
latest tap commit selected when preparation starts; if `main` changes while
building, restart from the latest commit. Check both repositories' Actions runs:
a successful source notification means the tap accepted the request, while the
tap run reports build, publication, and fresh-install results.

### Switching the tap to CI binaries

The generated `polaris-code.rb` is an alternative to the tap's current
source-and-bottle formula. A release archive is not a Homebrew bottle: bottles
also contain Homebrew's installation metadata. To reuse the application's CI
builds, copy the generated formula to the tap's `Formula/polaris-code.rb`
after publishing the GitHub release, and replace the tap's source-staging and
bottle-publication workflow with binary installation and `brew test` checks
on macOS and Linux. The existing workflow expects a source archive and cannot
consume this formula unchanged.

This handoff requires a separate tap change. The test and build workflow
generates the formula without writing to the tap or requiring a
cross-repository token. The existing Homebrew release notifier continues to
trigger the source-and-bottle workflow until the tap is migrated.
The generated formula supports only the two platforms in the build matrix
and has no source-build fallback. Keep the current tap formula if that
fallback is required, or extend the build matrix and generator before
migrating other platforms.
