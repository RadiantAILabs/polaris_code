# polaris_code

A composable Rust coding-agent library, also shipped as the `polaris`
terminal executable. Build agents from replaceable plugins, sandboxed tools,
and typed execution graphs.

```toml
[dependencies]
polaris_code = "0.1"
```

The library exposes the agent and tool plugins, model and permission
configuration, token accounting, and the shared turn driver:

```rust
use polaris_code::{AgentConfig, CodeAgentPlugin, CodeToolsPlugin, TokenMeter};
use polaris_code::run::drive_turn;
```

Compose plugins on a Polaris server, create a session, and drive a turn. The
[embedding guide](https://docs.rs/polaris_code/latest/polaris_code/docs/embedding/)
explains the required resources, input/output handling, permissions, and
cancellation. The [plugin catalog](https://docs.rs/polaris_code/latest/polaris_code/docs/plugins/)
lists the building blocks.

Supporting crates remain independently reusable: `polaris_cosmos` supplies
composition and evidence contracts, `polaris_ontrace` supplies the knowledge
graph, and `polaris-tui` supplies the terminal shell and panels.

See the [project repository](https://github.com/RadiantAILabs/polaris_code) for
CLI usage, architecture, benchmarks, and contribution instructions.

Licensed under Apache-2.0.
