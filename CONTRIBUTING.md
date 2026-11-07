# Contributing to polaris_code

Thanks for helping. This guide covers what we check when we review a pull
request, so you can get it right before you open one.

## Before you start

- **Bugs:** open an issue with steps to reproduce, the version, and the
  provider and model you used. If you already have a fix, say so in the issue.
- **Larger changes:** open an issue first and describe the design. A new
  plugin, tool, topology or provider is easier to agree on before the code
  exists.
- **Changes to Polaris itself:** this repo uses `polaris-ai` from crates.io.
  A change to framework types, model providers or core plugins belongs in
  [RadiantAILabs/polaris](https://github.com/RadiantAILabs/polaris), not here.
  Don't patch or fork `polaris-ai` in this repo.

Read [`AGENTS.md`](AGENTS.md) for the workspace layout and build commands.

## Checks to run

CI runs these. Run them locally first, with the pinned toolchain:

```bash
cargo +1.97.0 fmt --all -- --check
cargo +1.97.0 check --workspace --locked
cargo +1.97.0 test --workspace --locked
cargo +1.97.0 clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo +1.97.0 tree -i polaris-ai --locked   # must show exactly one polaris-ai
```

## Design: follow the philosophy

Read [`docs/philosophy.md`](docs/philosophy.md) and
[`docs/taxonomy.md`](docs/taxonomy.md) before a design change. We treat the
philosophy as a hard rule: a change that breaks it will not be merged, even if
it works.

In practice:

- **Add a capability as a plugin.** A new tool or feature is a self-contained
  module plus a plugin that declares what it provides and what it needs.
  Helper functions inside a capability don't need their own plugin.
- **Put the code in the right layer.** The taxonomy has three layers:
  composition root (`main.rs`, wiring), coding capabilities (tools,
  instrumentation, front ends) and agent and policy (`agents/`). Lower layers
  must not depend on upper ones. A tool must not read the agent's `LoopState`.
  The agent reaches tools through the tool registry, not their internals.
- **Use public contracts only.** Don't reach into another plugin's private
  types. Use its published resources and APIs.
- **Keep state and behavior apart.** State lives in typed resources; behavior
  lives in systems with explicit inputs (`Res<T>`, `ResMut<T>`, `Out<T>`).
  Avoid one large struct of optional values passed between steps.
- **Put control flow in the graph.** Retry, branch and stop decisions belong
  in graph edges (loops, conditional branches, error handlers), not in an
  `if` or `loop` inside a system body, and not in prompt wording that tells
  the model what to do.
- **Classify errors where they happen.** Decide whether an error can be
  retried at the point where it is raised, from structured data (an HTTP
  status, an error kind). Don't decide it later by searching the error text.
  An unknown error must not be retried.
- **Keep `main.rs` thin.** It chooses and wires plugins. Tool behavior and
  agent decisions belong in their own modules.
- **New topologies** go in `cosmos/graphs.rs` and are registered in
  `cosmos/topology.rs`. Don't add special cases to the host's selection loop.
- **Don't break provider swapping.** Code should work with every supported
  model provider, not just the default one.

## API quality

We review public Rust APIs against the
[Rust API Guidelines](https://rust-lang.github.io/api-guidelines/). The points
we see most often:

- **Naming:** no `get_` prefix on getters. Use Polaris suffixes: `XxxPlugin`,
  `XxxAPI`, `XxxNode`. Don't name error variables `e`; use `err`.
- **Types:** use enums instead of `bool` parameters, and newtypes for IDs.
  Explain any `serde_json::Value` or `Box<dyn Any>` in a public signature.
- **Errors:** error types implement `std::error::Error + Send + Sync`, with
  lowercase messages. No `unwrap()` or `expect()` on input from users, the
  model or tools.
- **Lints:** use `#[expect(lint, reason = "...")]`, not bare `#[allow]`.
- **Async:** don't block inside async code (`std::fs`, `std::thread::sleep`,
  `std::sync::Mutex`), and don't hold a lock guard across `.await`.
- **Common traits:** every new public type implements `Debug`, plus `Clone`,
  `PartialEq` and `Default` where they make sense.
- **Plugins:** list every dependency in `dependencies()`. Don't rely on
  registration order.

## Documentation

Every public item needs rustdoc. The build warns on missing docs, and clippy
treats warnings as errors.

A new **plugin**, **API** or **resource** must follow the standards in
[`docs/reference/plugins.md`](docs/reference/plugins.md),
[`docs/reference/api.md`](docs/reference/api.md) and
[`docs/reference/resources.md`](docs/reference/resources.md). In short:

- **Plugin:** what it does and when to use it; `# Resources Provided`,
  `# APIs Provided`, `# Dependencies` and `# Example`. Add `# Tools Provided`,
  `# Hooks Registered` or `# Lifecycle` when the plugin does those things.
- **API:** its purpose, the plugin that provides it, a table of its methods,
  when each method may be called, its composition rule, and an example that
  shows both providing and using it.
- **Resource:** its purpose, `Global` or `Local` with a reason, the plugin
  that provides it, how it may be read or changed, and an example `#[system]`
  that uses it.

Then add the item to the crate's catalog in `crates/<crate>/src/docs/`
(`plugins.md`, `apis.md` or `resources.md`). The catalog tests fail if you
forget:

```bash
cargo +1.97.0 test -p polaris_code --test plugin_catalog --test api_catalog --test resource_catalog
```

If your change adds a new way for users to integrate, add a row to
[`docs/reference/guide.md`](docs/reference/guide.md). Use intra-doc links
(``[`Type`](crate::path::Type)``) when referring to other types.

## Tests

A bug fix comes with a test that fails without the fix. New code comes with
tests for its main path and its error paths. We check that tests:

- **Assert the result, not just success.** Check the value inside `Ok`, and
  the specific error variant, not only `is_err()`.
- **Cover the branches** you added: empty input, missing values, boundaries.
- **Stand alone:** no shared mutable state, no order dependence. Clean up temp
  files, spawned tasks and environment changes.
- **Don't depend on timing or ordering:** no `sleep` followed by an assertion,
  no assertions on `HashMap` order.
- **Test at the right level:** test a system with the resources it needs, a
  plugin with only its declared dependencies, and a graph by checking which
  nodes ran.
- **Give useful failure messages** on non-trivial assertions.
- **Explain any `#[ignore]`:** say why it is ignored and when it can be turned
  back on.

## Security

The agent runs commands and edits files on the user's machine, so we review
these closely:

- Keep tools inside the configured working directory. Don't weaken path
  checks, symlink checks, command restrictions, secret redaction or spend
  limits without updating their tests and docs.
- New mutating tools must go through the permission policy, the edit journal
  and session review/rewind.
- No secrets in code, tests or fixtures. Don't log API keys, tokens or
  request bodies that contain credentials.
- Don't panic on input from users, the model or tools. Limit the size of
  anything read from them.
- New dependencies must come from crates.io, have a permissive license (MIT,
  Apache-2.0, BSD) and be justified. Avoid `unsafe`; if you need it, explain
  why it is sound.

The points above cover the code. For what the model may do through the agent,
and the rules for adding or changing a tool, read
[`docs/agent-safety.md`](docs/agent-safety.md).

## Pull requests

- Keep one change per pull request. Split unrelated changes.
- Commit messages are one short sentence.
- Describe what changed and why, and link the issue.
- Note breaking or notable changes in [`CHANGELOG.md`](CHANGELOG.md).
- Don't commit evaluation outputs, transcripts, logs, credentials or local
  environment files.

By contributing, you agree that your work is licensed under the
[Apache License 2.0](LICENSE).
