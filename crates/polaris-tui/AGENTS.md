# AGENTS.md — polaris-tui

`polaris-tui` is a reusable ratatui terminal shell for Polaris. Plugins
contribute in-process panels to a shared registry, and the shell owns the
terminal, focus, layout, and global key chords. It is one crate in the
`polaris_code` workspace; the root `AGENTS.md` covers the whole workspace.

## Where things are

- `README.md`: usage, key chords, and built-in panels.
- `docs/taxonomy.md`: the crate's layers (shell and panel contract, panels).
- `docs/philosophy.md`: the Polaris design principles this crate follows.
- `docs/reference/`: documentation standards for APIs, plugins, and resources.
- `src/docs/`: the API, plugin, and resource catalogs, checked by
  `tests/api_catalog.rs`, `tests/plugin_catalog.rs`, and
  `tests/resource_catalog.rs`.
- `docs/prompt-history-editor.md` and `docs/session-rewind.md`: feature notes.
- `src/panels/`: the built-in `conversation`, `logs`, `sessions`, and `help` panels.

## Build and test

Use the pinned toolchain for every Rust command:

```bash
cargo +1.97.0 check --workspace
cargo +1.97.0 test --workspace
cargo +1.97.0 fmt -- --check
cargo +1.97.0 clippy --all-targets --all-features -- -D warnings
```

To work on this crate alone, add `-p polaris-tui` to the `check`, `test`, and
`clippy` commands.

## Conventions

- Document public items; the workspace warns on missing documentation.
- Use `tracing` for diagnostics. The TUI owns stdout.
- Build custom widgets against the `polaris_tui::ratatui` re-export so the UI
  stack stays on one version.
- Do not add another `polaris-ai` source or change its version; the dependency
  is set once in the root `Cargo.toml`.
