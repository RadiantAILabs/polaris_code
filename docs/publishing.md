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

For each release, publish the reviewed source tag, download its GitHub source
archive, compute SHA-256 from those exact bytes, and update the formula's URL
and checksum together. The formula fetches the locked Cargo dependencies and
then installs offline. Its functional test checks the installed command's
version, help, and a real JSON topology inventory without a model key.

The tap's GitHub Actions workflow builds and tests bottles on Apple Silicon
macOS Sequoia and Linux x86-64. It validates the archive checksums, publishes
them as immutable GitHub release assets, and merges Homebrew's bottle metadata
into the formula. Separate fresh runners then install with `--force-bottle`,
verify the installation receipt, and run the functional tests. The macOS job
also checks that Golden Gate selects the compatible Apple Silicon bottle.

Update the formula's tagged source URL and checksum, or dispatch the tap's
workflow, to publish a new batch. Each batch uses its own download URL so
previous bottle assets are preserved. Bottles skip Rust and CMake build
dependencies during installation; platforms without a compatible bottle use
the formula's existing source-build path.
