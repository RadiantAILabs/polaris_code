//! `polaris` — a terminal coding agent driven by an ontology + knowledge graph.
//!
//! The binary is a thin caller of [`polaris_code::startup`], which boots a
//! Polaris `Server` with the ontrace RDF store as its spine: either an
//! interactive TUI conversation front end or a non-interactive `--print` front
//! end, both driving the same file + code-graph + shell + SPARQL toolsets and
//! the reflective `CodeAgent` through the shared run surface in
//! [`polaris_code::run`]. The store carries three named graphs — the `TBox`
//! (seeded ontology), the Code `ABox` (indexed repository), and the Session
//! `ABox` (the agent's reified trajectory).
//!
//! A program that wants this agent with more plugins added builds a
//! [`polaris_code::startup::Host`] of its own instead of copying this file.
//!
//! # Lifecycle
//!
//! `finish()` (build + ready: spawns the TUI in interactive mode, kicks code
//! indexing) → load seed ontologies → register the agent + create/resume a
//! session → drive the agent loop (looped for the TUI, once for `--print`) →
//! save the session → run bounded plugin cleanup on exit.
//!
//! # Usage
//!
//! ```bash
//! ANTHROPIC_API_KEY=… cargo run -p polaris_code --bin polaris -- [working_dir] [--session <id>]
//! ANTHROPIC_API_KEY=… cargo run -p polaris_code --bin polaris -- -p "add a README" --output-format json
//! ```

use polaris_code::startup::Host;

#[tokio::main]
async fn main() {
    let exit_code = Host::new().run(std::env::args_os()).await;
    // The agent loop holds long-lived clients (HTTP connection pools, the
    // oxigraph store) whose background workers can keep the tokio runtime from
    // draining promptly when it's dropped at the end of `main`. Plugins have
    // had their bounded cleanup opportunity, so exit directly rather than wait
    // indefinitely for the runtime itself to drain.
    std::process::exit(exit_code);
}
