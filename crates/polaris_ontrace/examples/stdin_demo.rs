//! Minimal demo: pipe trace lines on stdin; the plugin extracts triples,
//! writes them to the in-memory KG, and every 5s both prints the KG size
//! and snapshots the graph to `<dir>/<timestamp>.ttl` (atomic write — tail
//! with `watch -n 1 cat <dir>/<timestamp>.ttl`). On Ctrl-C the same dump
//! runs one final time.
//!
//! Usage:
//!
//! ```sh
//! # Default: writes to ./data/<timestamp>.ttl
//! cargo run --example stdin_demo
//!
//! # Custom output directory:
//! cargo run --example stdin_demo -- /tmp/ontrace
//! ```
//!
//! The directory is created if missing. Each run gets its own
//! timestamp-named file, so consecutive runs never clobber each other; the
//! periodic snapshots within a single run all overwrite that one file.
//!
//! ```sh
//! printf 'alice met bob\nbob works_at acme\n' | cargo run --example stdin_demo
//!
//! # Or pipe any upstream agent's NDJSON event stream:
//! some-agent --output-format stream-json | cargo run --example stdin_demo
//! ```

use polaris::models::{AnthropicPlugin, ModelsPlugin};
use polaris::plugins::PersistencePlugin;
use polaris::sessions::SessionsPlugin;
use polaris::sessions::store::memory::InMemoryStore;
use polaris::system::server::Server;
use polaris_ontrace::{ExtractionPlugin, KnowledgeGraph, StdinIOPlugin, TraceIngestPlugin, dump};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Default output directory if no positional arg is given.
const DEFAULT_DUMP_DIR: &str = "data";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter("info,polaris_ontrace=debug")
        .init();

    let dump_dir: PathBuf = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from(DEFAULT_DUMP_DIR), PathBuf::from);
    std::fs::create_dir_all(&dump_dir)?;

    // Filename-safe ISO 8601 (colons replaced with dashes so the path works
    // on every filesystem we care about).
    let stamp = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string();
    let dump_path = dump_dir.join(format!("{stamp}.ttl"));
    tracing::info!(path = %dump_path.display(), "dump file");

    let mut server = Server::new();
    server
        .add_plugins(PersistencePlugin)
        .add_plugins(ModelsPlugin)
        .add_plugins(AnthropicPlugin::from_env("ANTHROPIC_API_KEY"))
        .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
        .add_plugins(TraceIngestPlugin::new())
        .add_plugins(ExtractionPlugin::new())
        .add_plugins(StdinIOPlugin::new());
    server.finish().await?;

    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let ctx = server.create_context();
                if let Ok(kg) = ctx.get_resource::<KnowledgeGraph>() {
                    match write_dump(&kg, &dump_path) {
                        Ok(()) => tracing::info!(
                            triples = kg.len(),
                            path = %dump_path.display(),
                            "KG snapshot",
                        ),
                        Err(err) => tracing::warn!(
                            error = %err,
                            "periodic dump failed",
                        ),
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutdown requested");
                break;
            }
        }
    }

    let ctx = server.create_context();
    let kg = ctx.get_resource::<KnowledgeGraph>()?;
    write_dump(&kg, &dump_path)?;
    tracing::info!(path = %dump_path.display(), triples = kg.len(), "KG dumped");

    Ok(())
}

/// Write the KG to `path` atomically: serialize into `<path>.tmp` first,
/// then rename. Guarantees a concurrent reader never sees a half-written file
/// — important because the periodic snapshot fires while extraction is still
/// running.
fn write_dump(kg: &KnowledgeGraph, path: &Path) -> std::io::Result<()> {
    let tmp = path.with_extension("ttl.tmp");
    {
        let file = std::fs::File::create(&tmp)?;
        let mut writer = std::io::BufWriter::new(file);
        dump::write_turtle(kg, &mut writer)?;
        std::io::Write::flush(&mut writer)?;
    }
    std::fs::rename(&tmp, path)
}
