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
With `polaris-ai` 0.7.0, macOS can conservatively report
`cleanup_completed: false` after the last live process in a group exits:
Darwin rejects signaling a group containing only zombies. The consumer tests
check that the leader PID is fully reaped instead of equating that report with
a surviving process. The separate descendant-cleanup test still requires a
successful cleanup report and proves that a descendant ignoring `SIGTERM`
does not survive cancellation.

Documentation and branding changes run quick Python checks, skipping Rust
compilation, workspace tests, release builds, and Homebrew installation tests.
The Harbor adapter tests run on every change in their own job, against Harbor
0.24.0 and Python 3.12. That job fails if Harbor is missing instead of skipping.
The full pipeline runs when crate sources, tests, examples, benches, embedded
documentation, ontology inputs, evaluation code or fixtures, Cargo dependencies,
toolchain settings, or CI/build/release tooling change.
Pushes compare the entire before/after range; pull requests
compare against their merge base. If the range cannot be compared, CI runs
the full pipeline. Release tags and manual dispatches always run it too.

The final `CI` check verifies every job required for that change, including the
Harbor adapter tests; use it as the required status check in branch protection.
Documentation-only pushes do not cancel source builds already in progress. A
newer source build cancels the older job for the same platform and branch;
tagged jobs are retained.

The workspace consumes the published `polaris-ai` 0.8.0 dependency rather than
a developer's sibling checkout. The published 0.6.0 package lacks graph APIs
that this workspace already uses. Every workspace crate still inherits the
single root dependency.

The build matrix produces:

| Archive target | Minimum platform |
| --- | --- |
| `aarch64-apple-darwin` | Apple Silicon, macOS Sequoia 15 |
| `x86_64-unknown-linux-gnu` | Linux x86-64, glibc 2.35 (Ubuntu 22.04 baseline; Homebrew) |
| `x86_64-unknown-linux-musl` | Linux x86-64, static musl executable (Harbor and portable installs) |

Each `polaris-code-<version>-<target>.tar.gz` contains the executable, license,
README, and its logo assets, with a matching `.tar.gz.sha256` file. Each runner
extracts its archive and checks the version, help, and JSON topology inventory
without model credentials. The GNU archive retains its glibc baseline. The
musl archive is built by `scripts/build-portable-binary.sh` in its pinned Rust
Alpine image; it must have neither dynamic library dependencies nor a dynamic
loader. The archive smoke test verifies those properties before exercising
its version, help and topology inventory. Harbor downloads the musl archive:
benchmark task images can use an older glibc or an unidentified base image.
Homebrew continues to use the GNU archive.

To build the same portable executable locally, use an empty build directory
on a volume with room for Rust dependencies:

```bash
PORTABLE_TARGET_DIR=/path/to/build-target scripts/build-portable-binary.sh
```

The executable is at
`<build-target>/x86_64-unknown-linux-musl/release/polaris`. Release builds use
the application's default features: the Oxigraph store is in memory, and the
optional `polaris_ontrace/oxigraph-rocksdb` C++ backend is not linked into the
shipped binary. An all-features Clippy check separately covers that backend;
it does not change the release archive's feature selection.

Download the archives from the workflow's `polaris-<target>` artifacts within
14 days. Only after all checks and all three builds pass does the workflow
generate the `homebrew-formula` artifact. For a pushed tag in
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
`polaris` without Rust or CMake. On Linux, GCC supplies the runtime libraries
and `patchelf` adjusts the installed copy's library search path and optional
Homebrew glibc loader. When that loader is present, it also runs `patchelf`
with Homebrew's libraries so the tool can run on the Ubuntu 22.04 baseline.
The downloaded archive and its checksum stay intact.
To regenerate it locally after downloading both Homebrew archives and their
checksum files:

```bash
python3 scripts/homebrew_formula.py 0.3.0 RadiantAILabs/polaris_code dist
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

The tap's **Homebrew formula** workflow publishes the generated
`polaris-code.rb` from that release. It:

1. downloads the formula from the release;
2. checks that every URL points at that release, and that each archive
   matches the formula's SHA-256 and the release's `.sha256` file;
3. installs and tests the formula on fresh macOS and Linux runners;
4. commits it to the tap.

A release reaches the tap in a few minutes. The formula installs the prebuilt
archive for Apple Silicon on macOS Sequoia and newer, or Linux x86-64, with no
Rust toolchain or build step. Its functional test checks the installed
command's version, help, and a real JSON topology inventory without a model
key. The formula supports only those two platforms (Apple Silicon macOS and
Linux x86-64). On a Linux x86-64 system without a suitable glibc, use the static
musl archive; elsewhere, install with `cargo install polaris_code --locked`, or
extend the build matrix and the generator.

### One-time automation setup

Create a fine-grained GitHub personal access token owned by `RadiantAILabs`,
limited to the `homebrew-tap` repository, with **Actions: write** permission
(and automatic Metadata: read). Complete organization approval if required.
Store it as the `HOMEBREW_TAP_TOKEN` repository Actions secret in
[`RadiantAILabs/polaris_code` settings](https://github.com/RadiantAILabs/polaris_code/settings/secrets/actions).
Do not commit it or reuse a broad development credential. The source repository's
`GITHUB_TOKEN` cannot dispatch workflows in the separate tap. The tap uses its
own job-scoped token to push its formula; its repository
settings must allow Actions to write contents and push to `main`.

See GitHub's [workflow dispatch API permissions](https://docs.github.com/en/rest/actions/workflows#create-a-workflow-dispatch-event)
and [workflow event authentication rules](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/trigger-a-workflow).

### Retry and verify

Run the public repository's **Homebrew release** workflow manually with the
published `release_tag` to replay a notification. A formula that is already
current is not committed again, and an older version cannot replace a newer
formula. Enable `force_rebuild` to re-check and install-test the current
version; this also verifies authentication and release validation.

The tap's **Homebrew formula** workflow accepts the same `release_tag` and
`force_rebuild` inputs; with no tag, it checks and installs the current
formula. If the tap's `main` changes during a run, start a new run from the
latest commit. Check both repositories' Actions runs: a successful source
notification means the tap accepted the request, while the tap run reports
verification, installation, and publication results.
