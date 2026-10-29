# AGENTS.md — Polaris Consumer Project

> Copy this file to the root of any project that uses `polaris-ai` as a dependency.
> It provides the minimum an agent needs to navigate a consumer codebase. For full
> API reference, follow the links into [docs.rs/polaris-ai](https://docs.rs/polaris-ai).

## What is Polaris

Polaris is a Rust framework for building AI agents as directed graphs of async functions.
Agents are defined as **systems** (async fns with typed dependencies), composed into **graphs**
(control flow), and executed in **sessions** (stateful contexts).

**Crate:** `polaris-ai` (re-exports all sub-crates under `polaris_ai::*`)

## Import Conventions

```rust
// Prelude: Agent, Graph, system macro, tools, Out, Res, ResMut
use polaris_ai::prelude::*;

// Systems and resources
use polaris_ai::system::system;                       // #[system] macro
use polaris_ai::system::param::{Res, ResMut, Out, ErrOut, SystemContext};
use polaris_ai::system::resource::{GlobalResource, LocalResource};
use polaris_ai::system::server::Server;
use polaris_ai::system::plugin::{Plugin, PluginId, Version};
use polaris_ai::system::prelude::SystemError;

// Graph and Agent
use polaris_ai::graph::{Graph, GraphExecutor, CaughtError};
use polaris_ai::agent::{Agent, SetupError};

// Sessions
use polaris_ai::sessions::{SessionsAPI, SessionsPlugin, SessionId, AgentTypeId, InMemoryStore};

// Models / LLM
use polaris_ai::models::{ModelsPlugin, ModelRegistry, AnthropicPlugin};
use polaris_ai::models::llm::{Llm, LlmResponse, Message};

// Tools
use polaris_ai::tools::{ToolsPlugin, ToolRegistry, LlmReasonExt, LlmRequestBuilderExt};

// IO
use polaris_ai::plugins::{UserIO, IOMessage, IOContent, IOSource};

// HTTP — all surfaced through polaris-ai; no need to depend on inner crates
use polaris_ai::app::{AppPlugin, AppConfig, HttpRouter};
use polaris_ai::sessions::http::{HttpPlugin, HttpIOProvider};
```

## 5-Minute Mental Model

**Systems** are async functions. Dependencies are declared as typed parameters; no hidden state.

```rust
#[system]
async fn reason(config: Res<AppConfig>, mut memory: ResMut<Memory>) -> ReasoningResult {
    ReasoningResult { action: "search".into() }
}
```

**Resources** are typed state injected into systems:

| Type | Trait | Access | Registered via | Use for |
|------|-------|--------|---------------|---------|
| `Res<T>` | `GlobalResource` or `LocalResource` | Read-only | `server.insert_global(T)` or `ctx.insert(T)` | Config, registries, per-request input |
| `ResMut<T>` | `LocalResource` | Mutable | `server.register_local(\|\| T)` or `ctx.insert(T)` | Conversation history, accumulators |
| `Out<T>` | (any return type) | Read-only | Automatic from system return | Step-to-step data flow |
| `ErrOut<T>` | `ErrorContext` | Read-only | Automatic on error edges | Error handler systems |

**Graphs** compose systems with typed control flow — sequential, conditional, switch, loop, parallel, error.
See [`polaris_ai::graph`](https://docs.rs/polaris-ai/latest/polaris_ai/graph/index.html) for the full builder API.

**Sessions** bind a context + graph + executor for stateful agent execution.
See [`polaris_ai::sessions`](https://docs.rs/polaris-ai/latest/polaris_ai/sessions/index.html) for lifecycle, persistence, and one-shot vs multi-turn patterns.

## Data Flow — Preferred Patterns

### Pipeline (linear flows)

Each system returns a typed result; the next system reads it via `Out<T>`:

```rust
#[system] async fn validate(input: Res<MyInput>) -> ValidationResult { /* ... */ }
#[system] async fn transform(v: Out<ValidationResult>, cfg: Res<AppConfig>) -> TransformResult { /* ... */ }
#[system] async fn respond(t: Out<TransformResult>) -> FinalOutput { /* ... */ }

graph.add_system(validate).add_system(transform).add_system(respond);
```

### Accumulator (cross-cutting mutable state)

```rust
#[derive(Default, Clone)]
struct Issues(Vec<String>);
impl LocalResource for Issues {}

#[system] async fn step_a(mut issues: ResMut<Issues>) { issues.0.push("...".into()); }
#[system] async fn step_b(mut issues: ResMut<Issues>) { issues.0.push("...".into()); }

// Register as factory (fresh per context) or inject in setup closure:
server.register_local(Issues::default);
```

### Anti-pattern: god struct with Option fields

```rust
// ❌ Don't do this
struct WorkingState {
    input: Option<MyInput>,          // should be Res<MyInput>
    step_a_result: Option<StepA>,    // should be Out<StepA>
    step_b_result: Option<StepB>,    // should be Out<StepB>
    output: Option<FinalOutput>,     // should be the last system's return type
}
```

This defeats typed data flow. Use `Res<T>` for input, `Out<T>` for inter-system flow,
`ResMut<T>` only for genuinely accumulated state.

## "I Want To…" Semantic Map

| Goal | Approach | Reference |
|------|----------|-----------|
| **Define a system** | `#[system] async fn name(params) -> Output { }` | [`system`](https://docs.rs/polaris-ai/latest/polaris_ai/system/index.html) |
| **Pass data between systems** | Return `T` from A, declare `Out<T>` in B | [`system::param::Out`](https://docs.rs/polaris-ai/latest/polaris_ai/system/param/struct.Out.html) |
| **Read immutable input** | `ctx.insert(input)` in setup, read via `Res<T>` | [`system::param::Res`](https://docs.rs/polaris-ai/latest/polaris_ai/system/param/struct.Res.html) |
| **Accumulate mutable state** | `register_local(T::default)`, use `ResMut<T>` | [`system::param::ResMut`](https://docs.rs/polaris-ai/latest/polaris_ai/system/param/struct.ResMut.html) |
| **Branch on a condition** | `graph.add_conditional_branch::<T, _, _, _>(...)` | [`graph`](https://docs.rs/polaris-ai/latest/polaris_ai/graph/index.html) |
| **Multi-way branch** | `graph.add_switch::<T, _, _>(...)` | [`graph`](https://docs.rs/polaris-ai/latest/polaris_ai/graph/index.html) |
| **Loop until done** | `graph.add_loop::<State, _, _>(...)` | [`graph`](https://docs.rs/polaris-ai/latest/polaris_ai/graph/index.html) |
| **Run systems in parallel** | `graph.add_parallel(name, vec![...])` | [`graph`](https://docs.rs/polaris-ai/latest/polaris_ai/graph/index.html) |
| **Handle errors** | `.on_error(\|h\| ...)` or `graph.add_error_handler(...)` | [`graph::CaughtError`](https://docs.rs/polaris-ai/latest/polaris_ai/graph/struct.CaughtError.html) |
| **Timeout + retry** | `.system(f).with_timeout(d).with_retry(policy).done()` | [`graph`](https://docs.rs/polaris-ai/latest/polaris_ai/graph/index.html) |
| **Call an LLM** | `llm.builder().system(p).messages(m).generate().await` | [`models`](https://docs.rs/polaris-ai/latest/polaris_ai/models/index.html) |
| **Call LLM with tools** | `llm.builder().with_registry(&tools).reason().await` | [`tools::LlmReasonExt`](https://docs.rs/polaris-ai/latest/polaris_ai/tools/trait.LlmReasonExt.html) |
| **Define a tool** | `#[tool] async fn my_tool(param: String) -> String { }` | [`tools`](https://docs.rs/polaris-ai/latest/polaris_ai/tools/index.html) |
| **Define an agent** | `impl Agent { fn setup(...); fn build(graph); }` | [`agent::Agent`](https://docs.rs/polaris-ai/latest/polaris_ai/agent/trait.Agent.html) |
| **Register + run an agent** | `sessions.register_agent(A)?; sessions.process_turn(&id)` | [`sessions::SessionsAPI`](https://docs.rs/polaris-ai/latest/polaris_ai/sessions/struct.SessionsAPI.html) |
| **One-shot request** | `sessions.run_oneshot(&agent_type, \|ctx\| ...)` | [`sessions`](https://docs.rs/polaris-ai/latest/polaris_ai/sessions/index.html) |
| **Multi-turn w/ cleanup** | `sessions.scoped_session(&agent_type, \|ctx\| ...)` | [`sessions`](https://docs.rs/polaris-ai/latest/polaris_ai/sessions/index.html) |
| **Inject per-turn data** | `sessions.process_turn_with(&id, \|ctx\| ctx.insert(T))` | [`sessions`](https://docs.rs/polaris-ai/latest/polaris_ai/sessions/index.html) |
| **Extract output after turn** | `sessions.with_context(&id, \|ctx\| ctx.get_output::<T>())` | [`sessions`](https://docs.rs/polaris-ai/latest/polaris_ai/sessions/index.html) |
| **Serve over HTTP** | `AppPlugin` + `HttpRouter` + `HttpPlugin` | [`app`](https://docs.rs/polaris-ai/latest/polaris_ai/app/index.html) |
| **Register custom routes** | `add_routes(router)` (stateless) or `add_routes_with(\|server\| ...)` (stateful) in `build()` | [`app::HttpRouter`](https://docs.rs/polaris-ai/latest/polaris_ai/app/struct.HttpRouter.html) |
| **Access APIs in handlers** | `add_routes_with` closure runs in `AppPlugin::ready()`; use `.with_state(api)` and `State<T>` in handlers | [`app`](https://docs.rs/polaris-ai/latest/polaris_ai/app/index.html) |
| **Bridge HTTP → agent IO** | `HttpIOProvider::new(1)` → channels → `UserIO` | [`sessions::http::HttpIOProvider`](https://docs.rs/polaris-ai/latest/polaris_ai/sessions/http/struct.HttpIOProvider.html) |

## Feature Flags

Every feature a consumer needs is surfaced through `polaris-ai` directly — you
do not need to depend on the inner crates (`polaris_app`, `polaris_sessions`, …).

```toml
[dependencies]
polaris-ai = { version = "0.7", features = ["anthropic", "sessions-http"] }
```

| Feature | Enables |
|---------|---------|
| `anthropic` / `openai` / `bedrock` | Respective LLM provider |
| `sessions-http` | REST endpoints for session management |
| `file-store` *(default)* | Filesystem-backed session store; `default-features = false` to opt out |
| `otel` | OpenTelemetry OTLP exporter |
| `tiktoken` | BPE token counting |
| `dashboard` | HTTP surfaces on `ToolsPlugin` / `ModelsPlugin` / `TracingPlugin` for an external dashboard frontend (frozen tool & model snapshots, `/v1/tracing/*` endpoints) |
| `typegen` | Generates TypeScript bindings (`bindings/ts/src/`) for canonical Polaris types via `ts-rs` |
| `test-utils` | `MockClock` / `MockIOProvider` mocks for downstream `dev-dependencies` |

Full list: [crate-level docs](https://docs.rs/polaris-ai/latest/polaris_ai/#feature-flags).

## Testing

```rust
#[tokio::test]
async fn test_my_system() {
    let ctx = SystemContext::new()
        .with(MyConfig { value: 42 })
        .with(Memory::default());
    let result = my_system.run(&ctx).await.unwrap();
    assert_eq!(result.output, "expected");
}
```

For graph-level tests, build a minimal `Server`, register the agent, and run a turn.
See [`sessions`](https://docs.rs/polaris-ai/latest/polaris_ai/sessions/index.html) for the full testing pattern.

## Where To Go For More

All per-module documentation lives on docs.rs:

| Topic | Link |
|-------|------|
| Crate overview (layers, quick start, data flow) | [`polaris_ai`](https://docs.rs/polaris-ai/latest/polaris_ai/) |
| System framework (systems, resources, plugins, server) | [`polaris_ai::system`](https://docs.rs/polaris-ai/latest/polaris_ai/system/index.html) |
| Graph execution (nodes, edges, executor, hooks) | [`polaris_ai::graph`](https://docs.rs/polaris-ai/latest/polaris_ai/graph/index.html) |
| Agent trait | [`polaris_ai::agent`](https://docs.rs/polaris-ai/latest/polaris_ai/agent/index.html) |
| Sessions and lifecycle | [`polaris_ai::sessions`](https://docs.rs/polaris-ai/latest/polaris_ai/sessions/index.html) |
| LLM providers and model registry | [`polaris_ai::models`](https://docs.rs/polaris-ai/latest/polaris_ai/models/index.html) |
| Tools (`#[tool]` macro, registry) | [`polaris_ai::tools`](https://docs.rs/polaris-ai/latest/polaris_ai/tools/index.html) |
| Core plugins (IO, tracing, time, persistence) | [`polaris_ai::plugins`](https://docs.rs/polaris-ai/latest/polaris_ai/plugins/index.html) |
| HTTP runtime | [`polaris_ai::app`](https://docs.rs/polaris-ai/latest/polaris_ai/app/index.html) |
| Shell execution | [`polaris_ai::shell`](https://docs.rs/polaris-ai/latest/polaris_ai/shell/index.html) |
