//! `ontrace` — extract a knowledge graph from a live trace stream, or work
//! with a saved one.
//!
//! ```sh
//! ontrace                          # tail the newest Claude Code session under ~/.claude/projects/
//! ontrace <session-id>             # tail that Claude Code session (UUID) wherever it lives
//! ontrace tail [SOURCE] [OPTS]     # explicit tail subcommand — same as the bare form
//! ontrace inspect <ttl-or-dir>     # load a saved KG and open the polaris-tui shell
//!                                  #   (chat + SPARQL + logs + help panels)
//! ontrace inspect <ttl> --ask Q    # one-shot question, prints the answer, no TUI
//! ontrace query <ttl-or-dir> [Q]   # run raw SPARQL against a saved KG (no LLM)
//! ontrace list                     # print known Claude Code sessions, newest first
//! ```
//!
//! `inspect` / `query` take a positional `<ttl-or-dir>`: a single `.ttl`
//! file, or a directory whose every `.ttl` is loaded. `inspect` lands it
//! in the named data graph; `query` lands it in the default graph so bare
//! `{ ?s ?p ?o }` patterns just work. `--seed` / `--seed-dir` still feed
//! the *ontology* graph alongside it.
//!
//! The `tail` subcommand owns the source flags (`--stdin`, `--file`,
//! `--otlp`, or a positional session id) and may combine `--otlp` with one
//! primary source. Every 5s (and once on Ctrl-C) the KG is dumped to
//! `<out>/<timestamp>.ttl` atomically.
//!
//! With the `oxigraph` cargo feature on (default), extraction lands in an
//! oxigraph store split into ontology (`TBox`) and data (`ABox`) named
//! graphs; the periodic snapshot dumps the data graph. `--backend memory`
//! opts down to the naive in-memory `KnowledgeGraph` (Vec<Triple>) path.
//! `--backend oxigraph:<dir>` (requires the `oxigraph-rocksdb` feature)
//! uses a RocksDB-backed store at `<dir>` for durability across restarts.
//!
//! `inspect` / `query` require the `oxigraph` cargo feature; without it the
//! subcommands parse but exit with an explanatory error.

use clap::{Args, Parser, Subcommand};
#[cfg(feature = "oxigraph")]
use oxigraph::model::GraphNameRef;
use polaris::models::{AnthropicPlugin, ModelsPlugin};
use polaris::plugins::PersistencePlugin;
use polaris::sessions::SessionsPlugin;
use polaris::sessions::store::memory::InMemoryStore;
use polaris::system::server::Server;
#[cfg(feature = "oxigraph")]
use polaris::tools::{ToolPermission, ToolsPlugin};
use polaris_ontrace::sources::claude_code;
use polaris_ontrace::{
    ClaudeCodeIOPlugin, ExtractionPlugin, FileTailIOPlugin, KnowledgeGraph, OtlpIOPlugin,
    SeedOntology, Sightings, Spend, StdinIOPlugin, TraceIngestPlugin, dump, load_alias_seeds,
};
#[cfg(feature = "oxigraph")]
use polaris_ontrace::{
    FilteredLogPanelPlugin, InspectAPI, InspectPlugin, InspectTuiPlugin, OxigraphExtractionPlugin,
    oxigraph::{
        OxigraphPlugin, OxigraphStore, canonicalize_default_graph, canonicalize_named_graph,
    },
};
#[cfg(feature = "oxigraph")]
use polaris_tui::panels::HelpPanelPlugin;
#[cfg(feature = "oxigraph")]
use polaris_tui::{LayoutMode, TuiConfig, TuiHandle, TuiPlugin};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Default output directory for periodic KG snapshots.
const DEFAULT_DUMP_DIR: &str = "data";

/// Default `--backend` value — oxigraph when the feature is compiled in,
/// otherwise the always-available naive in-memory backend.
#[cfg(feature = "oxigraph")]
const DEFAULT_BACKEND: &str = "oxigraph";
/// Default `--backend` value — see the `oxigraph`-feature variant.
#[cfg(not(feature = "oxigraph"))]
const DEFAULT_BACKEND: &str = "memory";

/// Top-level CLI: an optional subcommand. The bare form (no subcommand) and
/// a bare positional UUID both fall through to `tail` against the newest /
/// named Claude Code session.
#[derive(Parser)]
#[command(
    name = "ontrace",
    version,
    about = "Build a temporal knowledge graph from trace streams"
)]
struct Cli {
    /// Chosen subcommand. `None` means "tail the newest Claude Code session".
    #[command(subcommand)]
    command: Option<Command>,
}

/// The mutually-exclusive things `ontrace` can do.
#[derive(Subcommand)]
enum Command {
    /// Tail a trace stream and extract a knowledge graph.
    Tail(TailArgs),
    /// Load a saved KG and answer questions about it with a SPARQL-tool agent.
    Inspect(InspectArgs),
    /// Run raw SPARQL against a saved KG — no LLM, no API key needed.
    Query(QueryArgs),
    /// List known Claude Code sessions, newest first.
    List,
    /// Bare `ontrace <session-id>` — captured here, routed to `tail`.
    #[command(external_subcommand)]
    External(Vec<String>),
}

/// `--seed` / `--seed-dir` / `--packs` — shared by every subcommand that
/// wants a curated ontology. `clap` enforces the `--seed` xor `--seed-dir`
/// and `--packs` requires `--seed-dir` constraints at parse time.
#[derive(Args, Clone, Default)]
struct SeedArgs {
    /// Load a single `.ttl` file as the curated `SeedOntology`.
    #[arg(long, value_name = "PATH", conflicts_with = "seed_dir")]
    seed: Option<PathBuf>,
    /// Load every `.ttl` in a directory and merge them.
    #[arg(long = "seed-dir", value_name = "DIR")]
    seed_dir: Option<PathBuf>,
    /// With `--seed-dir`, restrict the merge to these pack names.
    #[arg(
        long,
        value_name = "A,B,C",
        value_delimiter = ',',
        requires = "seed_dir"
    )]
    packs: Option<Vec<String>>,
}

/// Source + backend flags for the `tail` subcommand.
#[derive(Args)]
struct TailArgs {
    /// Claude Code session id (UUID) to tail. Omit to tail the newest session.
    #[arg(value_name = "SESSION_ID", conflicts_with_all = ["stdin", "file"])]
    session: Option<String>,
    /// Read trace lines piped on stdin.
    #[arg(long, conflicts_with = "file")]
    stdin: bool,
    /// Tail an arbitrary newline-delimited file.
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,
    /// Also accept OTLP/HTTP traces. Bare flag binds 127.0.0.1:4318.
    // keep the literal in sync with `polaris_ontrace::sources::otlp::DEFAULT_BIND`.
    #[arg(long, value_name = "BIND", num_args = 0..=1, default_missing_value = "127.0.0.1:4318")]
    otlp: Option<SocketAddr>,
    /// KG snapshot output directory.
    #[arg(long = "out", value_name = "DIR", default_value = DEFAULT_DUMP_DIR)]
    out_dir: PathBuf,
    /// KG backend: `oxigraph`, `oxigraph:<dir>`, or `memory`.
    #[arg(long, value_name = "KIND", default_value_t = DEFAULT_BACKEND.to_owned())]
    backend: String,
    /// Override the extractor LLM model id.
    #[arg(long, value_name = "ID")]
    model: Option<String>,
    /// Load `alias|target` pairs from a file and pre-seed the [`Aliases`]
    /// table. See `ontologies/aliases.txt` for the format.
    #[arg(long, value_name = "PATH")]
    aliases: Option<PathBuf>,
    /// Curated ontology selection.
    #[command(flatten)]
    seed: SeedArgs,
}

impl TailArgs {
    /// Build the default `tail` configuration (newest session, or the named
    /// `session` if given) — used for the bare `ontrace` / `ontrace <uuid>`
    /// forms that don't go through the `tail` subcommand parser.
    fn defaults(session: Option<String>) -> Self {
        Self {
            session,
            stdin: false,
            file: None,
            otlp: None,
            out_dir: PathBuf::from(DEFAULT_DUMP_DIR),
            backend: DEFAULT_BACKEND.to_owned(),
            model: None,
            aliases: None,
            seed: SeedArgs::default(),
        }
    }
}

/// Positional + flags for the `inspect` subcommand.
#[derive(Args)]
struct InspectArgs {
    /// A `.ttl` file, or a directory whose every `.ttl` is loaded into the
    /// data graph.
    #[arg(value_name = "TTL_OR_DIR")]
    input: PathBuf,
    /// Ask one question and exit, instead of opening the polaris-tui shell.
    #[arg(long, value_name = "QUESTION")]
    ask: Option<String>,
    /// Override the inspect-agent LLM model id.
    #[arg(long, value_name = "ID")]
    model: Option<String>,
    /// Skip the `owl:sameAs` canonicalization pre-pass over the data graph.
    /// Mirrors `query --raw`; by default every non-`owl:sameAs` triple is
    /// rewritten to its canonical-cluster form so agent SPARQL doesn't have
    /// to dance around alias drift.
    #[arg(long = "raw", alias = "no-canonicalize")]
    raw: bool,
    /// Curated ontology to load into the ontology graph alongside the input.
    #[command(flatten)]
    seed: SeedArgs,
}

/// Positional + flags for the `query` subcommand.
#[derive(Args)]
struct QueryArgs {
    /// A `.ttl` file, or a directory whose every `.ttl` is loaded into the
    /// data graph.
    #[arg(value_name = "TTL_OR_DIR")]
    input: PathBuf,
    /// SPARQL query to run. Omit to open a one-query-per-line stdin REPL.
    #[arg(value_name = "SPARQL")]
    query: Option<String>,
    /// Skip the `owl:sameAs` canonicalization pre-pass. By default the loader
    /// rewrites every non-`owl:sameAs` triple so its subject and object are
    /// the cluster's canonical IRI — `GROUP BY` / `COUNT` over `?s` then
    /// reflect entity-level totals instead of double-counting each alias.
    /// Pass `--raw` to query the dataset exactly as it sits on disk.
    #[arg(long = "raw", alias = "no-canonicalize")]
    raw: bool,
    /// Curated ontology to load into the ontology graph alongside the input.
    #[command(flatten)]
    seed: SeedArgs,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();

    // Skip the stderr-bound `fmt` subscriber when we're about to launch the
    // TUI — [`FilteredLogPanelPlugin`] installs its own buffer-backed global
    // subscriber and a second `init()` would silently break it.
    if !is_tui_mode(&cli.command) {
        tracing_subscriber::fmt()
            .with_env_filter("info,polaris_ontrace=debug")
            .init();
    }

    match cli.command {
        None => run_tail(TailArgs::defaults(None)).await,
        Some(Command::Tail(args)) => run_tail(args).await,
        Some(Command::List) => list_sessions(),
        Some(Command::External(tokens)) => match tokens.as_slice() {
            [one] if !one.starts_with('-') => run_tail(TailArgs::defaults(Some(one.clone()))).await,
            _ => Err(format!("unknown subcommand: {}", tokens.join(" ")).into()),
        },
        #[cfg(feature = "oxigraph")]
        Some(Command::Inspect(args)) => run_inspect(args).await,
        #[cfg(not(feature = "oxigraph"))]
        Some(Command::Inspect(_)) => {
            Err("`ontrace inspect` requires the `oxigraph` cargo feature".into())
        }
        #[cfg(feature = "oxigraph")]
        Some(Command::Query(args)) => run_query(args),
        #[cfg(not(feature = "oxigraph"))]
        Some(Command::Query(_)) => {
            Err("`ontrace query` requires the `oxigraph` cargo feature".into())
        }
    }
}

/// True if `command` will hand control to the polaris-tui shell and the
/// global tracing subscriber should be left for [`FilteredLogPanelPlugin`] to claim.
fn is_tui_mode(command: &Option<Command>) -> bool {
    #[cfg(feature = "oxigraph")]
    if let Some(Command::Inspect(args)) = command {
        return args.ask.is_none();
    }
    let _ = command;
    false
}

/// Run the `tail` flow: wire the source + backend plugins, finish the
/// server, then dump the KG every 5s and once more on Ctrl-C.
async fn run_tail(args: TailArgs) -> Result<(), Box<dyn std::error::Error>> {
    let backend = parse_backend(&args.backend)?;

    std::fs::create_dir_all(&args.out_dir)?;
    let stamp = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string();
    let dump_path = args.out_dir.join(format!("{stamp}.ttl"));
    let pretty_path = args.out_dir.join(format!("{stamp}.md"));
    tracing::info!(
        ttl = %dump_path.display(),
        md = %pretty_path.display(),
        "snapshot targets",
    );

    let mut server = Server::new();
    server
        .add_plugins(PersistencePlugin)
        .add_plugins(ModelsPlugin)
        .add_plugins(AnthropicPlugin::from_env("ANTHROPIC_API_KEY"))
        .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
        .add_plugins(TraceIngestPlugin::new());

    let seed = args.seed.resolve()?;
    if let Some(s) = &seed {
        tracing::info!(
            name = %s.name,
            predicates = s.predicates.len(),
            classes = s.classes.len(),
            "loaded seed ontology",
        );
    }
    let alias_seeds = match &args.aliases {
        Some(path) => {
            let pairs =
                load_alias_seeds(path).map_err(|e| format!("--aliases {}: {e}", path.display()))?;
            tracing::info!(
                path = %path.display(),
                pairs = pairs.len(),
                "loaded alias seeds",
            );
            pairs
        }
        None => Vec::new(),
    };
    register_backend(
        &mut server,
        &backend,
        seed,
        args.model.as_deref(),
        &alias_seeds,
    );
    register_sources(&mut server, &args)?;

    server.finish().await?;

    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                if let Err(err) = snapshot(&server, &backend, &dump_path, &pretty_path) {
                    tracing::warn!(error = %err, "periodic dump failed");
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutdown requested");
                break;
            }
        }
    }

    snapshot(&server, &backend, &dump_path, &pretty_path)?;
    tracing::info!(
        ttl = %dump_path.display(),
        md = %pretty_path.display(),
        "final snapshot dumped",
    );

    print_spend_summary(&server);

    Ok(())
}

/// Print a per-model token / cost breakdown for the run, plus the total.
///
/// Reads the `Spend` global registered by whichever extraction backend was
/// installed. Unknown / unpriced models are surfaced as a warning so the
/// total isn't silently underreported.
fn print_spend_summary(server: &Server) {
    let ctx = server.create_context();
    let Ok(spend) = ctx.get_resource::<Spend>() else {
        return;
    };
    let report = spend.report();
    if report.is_empty() {
        return;
    }
    let (total, unpriced) = spend.total_usd();
    eprintln!("\nLLM spend:");
    for row in &report {
        let cost = row
            .cost_usd
            .map_or_else(|| "  (no price)".to_owned(), |c| format!("  ${c:.4}"));
        eprintln!(
            "  {:<40}  calls={:>5}  in={:>8}  out={:>8}{}",
            row.model_id, row.usage.calls, row.usage.input_tokens, row.usage.output_tokens, cost,
        );
    }
    eprintln!("  total: ${total:.4} USD");
    if !unpriced.is_empty() {
        eprintln!(
            "  note: {} unpriced model(s) excluded from total: {}",
            unpriced.len(),
            unpriced.join(", "),
        );
    }
}

/// Load the `inspect` / `query` positional input into `into_graph`: a
/// single `.ttl` file, or every `.ttl` in a directory.
///
/// `inspect` passes the named data graph (its agent prompt references that
/// IRI); `query` passes [`GraphNameRef::DefaultGraph`] so bare `{ ?s ?p ?o }`
/// patterns resolve without a `GRAPH` wrapper.
///
/// Each file is parsed independently by `OxigraphStore::load_turtle`, so —
/// unlike seed-pack merging — there's no cross-file `@prefix` collision to
/// resolve. Returns the number of files loaded.
///
/// # Errors
///
/// Errors if `input` is an empty / `.ttl`-free directory, or if any file
/// fails to read or parse.
#[cfg(feature = "oxigraph")]
fn load_input(
    store: &OxigraphStore,
    input: &Path,
    into_graph: GraphNameRef<'_>,
) -> Result<usize, Box<dyn std::error::Error>> {
    let mut files: Vec<PathBuf> = Vec::new();
    if input.is_dir() {
        for entry in std::fs::read_dir(input)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "ttl") {
                files.push(path);
            }
        }
        files.sort();
        if files.is_empty() {
            return Err(format!("no .ttl files found in {}", input.display()).into());
        }
    } else {
        files.push(input.to_path_buf());
    }
    for path in &files {
        let turtle =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        store
            .load_turtle(&turtle, into_graph)
            .map_err(|e| format!("load {}: {e}", path.display()))?;
    }
    Ok(files.len())
}

/// Inspect-mode entry point.
///
/// Two paths, picked by `--ask`:
///
/// - `--ask <q>` runs one question, prints the answer to stdout, exits.
///   No TUI, no logs swallowed — drop-in for shell pipelines and tests.
/// - Otherwise launches the `polaris_tui` shell with three panels:
///   [`InspectChatPanel`](polaris_ontrace::InspectChatPanel) (agent chat),
///   [`SparqlPanel`](polaris_ontrace::SparqlPanel) (raw SPARQL),
///   plus [`FilteredLogPanelPlugin`](polaris_ontrace::FilteredLogPanelPlugin)
///   (target-filtered to polaris-only logs) and
///   [`HelpPanelPlugin`](polaris_tui::panels::HelpPanelPlugin) from polaris-tui.
///
/// Tool permissions are forced to `Allow` because the operator is sitting at
/// the terminal driving the agent — confirmation prompts would just stall the
/// loop. The store is in-memory and disposable, so a mutating SPARQL update
/// can't damage anything beyond the live session.
#[cfg(feature = "oxigraph")]
async fn run_inspect(args: InspectArgs) -> Result<(), Box<dyn std::error::Error>> {
    let seed = args.seed.resolve()?;
    if let Some(s) = &seed {
        tracing::info!(
            name = %s.name,
            predicates = s.predicates.len(),
            classes = s.classes.len(),
            "loaded seed ontology for inspect",
        );
    }

    if args.ask.is_some() {
        return run_inspect_oneshot(args, seed).await;
    }
    run_inspect_tui(args, seed).await
}

/// One-shot `--ask` path: no TUI, just print the answer.
#[cfg(feature = "oxigraph")]
async fn run_inspect_oneshot(
    args: InspectArgs,
    seed: Option<SeedOntology>,
) -> Result<(), Box<dyn std::error::Error>> {
    let question = args.ask.clone().expect("caller checked args.ask.is_some()");
    let canonicalize = !args.raw;
    let mut server = Server::new();
    register_inspect_plugins(&mut server, &args);
    server.finish().await?;
    load_inspect_inputs(&server, &args.input, seed.as_ref(), canonicalize)?;

    let api = server
        .api::<InspectAPI>()
        .ok_or("InspectAPI not published — InspectPlugin missing?")?
        .clone();
    let answer = api.ask(question).await?;
    println!("{answer}");
    print_spend_summary(&server);
    Ok(())
}

/// Interactive path: launch the polaris-tui shell with chat + SPARQL + logs
/// + help panels and block until the user quits (`Ctrl-C` / `Ctrl-Q`).
///
/// The TUI plugins are registered *before* `finish()` so polaris-tui's
/// runtime spawns with the panels already drained from the registry. The
/// input file + seed are loaded *after* `finish()` — the
/// [`OxigraphStore`] global only materialises during the build phase
/// `finish()` drives.
#[cfg(feature = "oxigraph")]
async fn run_inspect_tui(
    args: InspectArgs,
    seed: Option<SeedOntology>,
) -> Result<(), Box<dyn std::error::Error>> {
    let canonicalize = !args.raw;
    let mut server = Server::new();
    register_inspect_plugins(&mut server, &args);
    server
        .add_plugins(TuiPlugin::new(
            TuiConfig::default().with_layout(LayoutMode::Tiled),
        ))
        .add_plugins(FilteredLogPanelPlugin::default())
        .add_plugins(HelpPanelPlugin)
        .add_plugins(InspectTuiPlugin::new());
    server.finish().await?;
    load_inspect_inputs(&server, &args.input, seed.as_ref(), canonicalize)?;

    let wait = server
        .api::<TuiHandle>()
        .ok_or("TuiHandle not published — TuiPlugin missing?")?
        .wait();
    wait.await;
    print_spend_summary(&server);
    Ok(())
}

/// Wire persistence, sessions, tools, oxigraph (in-memory, mutating
/// permissions on), and [`InspectPlugin`] onto `server`. No `finish()` call —
/// the caller adds TUI plugins (or not) and then drives the build.
#[cfg(feature = "oxigraph")]
fn register_inspect_plugins(server: &mut Server, args: &InspectArgs) {
    let oxi = OxigraphPlugin::in_memory()
        .with_tools(true)
        .with_update_permission(ToolPermission::Allow)
        .with_load_permission(ToolPermission::Allow);
    let mut inspect_plugin = InspectPlugin::new();
    if let Some(m) = &args.model {
        inspect_plugin = inspect_plugin.with_model(m);
    }
    server
        .add_plugins(PersistencePlugin)
        .add_plugins(ModelsPlugin)
        .add_plugins(AnthropicPlugin::from_env("ANTHROPIC_API_KEY"))
        .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
        .add_plugins(ToolsPlugin)
        .add_plugins(oxi)
        .add_plugins(inspect_plugin);
}

/// Load `input` into the data graph and any `seed` ontology into the
/// ontology graph. Must be called *after* `server.finish()`.
#[cfg(feature = "oxigraph")]
fn load_inspect_inputs(
    server: &Server,
    input: &Path,
    seed: Option<&SeedOntology>,
    canonicalize: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let ctx = server.create_context();
    let store = ctx.get_resource::<OxigraphStore>()?;
    load_input(&store, input, store.data_graph().into())?;
    if let Some(s) = seed {
        store
            .load_turtle(&s.turtle, store.ontology_graph())
            .map_err(|e| format!("load seed into ontology graph: {e}"))?;
    }
    if canonicalize {
        canonicalize_named_graph(&store, store.data_graph().as_str())
            .map_err(|e| format!("canonicalize sameAs closure: {e}"))?;
    }
    tracing::info!(
        input = %input.display(),
        quads = store.len()?,
        data_graph = %store.data_graph().as_str(),
        ontology_graph = %store.ontology_graph().as_str(),
        canonicalized = canonicalize,
        "inspect store ready",
    );
    Ok(())
}

/// Query-mode entry point: load `args.input` into a fresh in-memory oxigraph
/// store and run raw SPARQL — no `Server`, no LLM, no API key. With a
/// positional query it runs once; without one it opens a stdin REPL.
#[cfg(feature = "oxigraph")]
fn run_query(args: QueryArgs) -> Result<(), Box<dyn std::error::Error>> {
    let seed = args.seed.resolve()?;
    let store = OxigraphStore::in_memory()?;
    load_input(&store, &args.input, GraphNameRef::DefaultGraph)?;
    if let Some(s) = &seed {
        store
            .load_turtle(&s.turtle, store.ontology_graph())
            .map_err(|e| format!("load seed into ontology graph: {e}"))?;
    }
    if !args.raw {
        canonicalize_default_graph(&store)
            .map_err(|e| format!("canonicalize sameAs closure: {e}"))?;
    }
    tracing::info!(
        input = %args.input.display(),
        quads = store.len()?,
        data_graph = "default",
        ontology_graph = %store.ontology_graph().as_str(),
        canonicalized = !args.raw,
        "query store ready",
    );

    match args.query {
        Some(sparql) => {
            println!("{}", run_one_query(&store, &sparql, seed.as_ref())?);
            Ok(())
        }
        None => query_repl(&store, seed.as_ref()),
    }
}

/// Execute one SPARQL query against `store` and render the result.
///
/// When a `seed` ontology is loaded its `@prefix` declarations are
/// prepended as a SPARQL `PREFIX` prologue (see
/// [`SeedOntology::sparql_prologue`]), so a query can use the curated
/// pack's compact names without redeclaring them. Prefixes the query
/// already declares are left untouched.
///
/// `CONSTRUCT` / `DESCRIBE` results serialize as Turtle; `SELECT` / `ASK`
/// results serialize as SPARQL Results JSON — the same forms the inspect
/// agent's `sparql_query` tool returns, so queries prototyped here transfer
/// straight over.
#[cfg(feature = "oxigraph")]
fn run_one_query(
    store: &OxigraphStore,
    sparql: &str,
    seed: Option<&SeedOntology>,
) -> Result<String, Box<dyn std::error::Error>> {
    use oxigraph::io::RdfFormat;
    use oxigraph::sparql::QueryResults;
    use oxigraph::sparql::results::QueryResultsFormat;

    let prologue = seed.map(|s| s.sparql_prologue(sparql)).unwrap_or_default();
    let effective = format!("{prologue}{sparql}");
    let results = store.query(&effective)?;
    let bytes = match results {
        QueryResults::Graph(_) => results.write_graph(Vec::<u8>::new(), RdfFormat::Turtle)?,
        QueryResults::Solutions(_) | QueryResults::Boolean(_) => {
            results.write(Vec::<u8>::new(), QueryResultsFormat::Json)?
        }
    };
    Ok(String::from_utf8(bytes)?)
}

/// Read one SPARQL query per line from stdin, print results, exit on EOF.
///
/// `PREFIX` declarations are *sticky*: a line may consist of one or more
/// leading `PREFIX name: <iri>` clauses, which are remembered for the rest
/// of the session and prepended to every subsequent query. A line that is
/// nothing but `PREFIX` clauses just updates the table and prints it back —
/// no query runs. Re-declaring a name overwrites it (last write wins), and
/// session prefixes shadow seed-pack prefixes of the same name.
///
/// A query error is printed and the loop continues — a typo shouldn't end
/// the session.
#[cfg(feature = "oxigraph")]
fn query_repl(
    store: &OxigraphStore,
    seed: Option<&SeedOntology>,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::collections::BTreeMap;
    use std::io::{BufRead, Write};

    eprintln!(
        "ontrace query — one SPARQL query per line. Ctrl-D to quit.\n  \
         data is in the default graph; curated ontology in <{}>.\n  \
         PREFIX declarations are remembered across lines.",
        store.ontology_graph().as_str(),
    );
    if let Some(s) = seed
        && !s.prefixes.is_empty()
    {
        let mut names: Vec<&str> = s.prefixes.keys().map(String::as_str).collect();
        names.sort_unstable();
        eprintln!(
            "  seed prefixes available without declaring them: {}",
            names.join(", "),
        );
    }
    let stdin = std::io::stdin();
    let mut line = String::new();
    let mut session: BTreeMap<String, String> = BTreeMap::new();
    loop {
        eprint!("sparql> ");
        std::io::stderr().flush().ok();
        line.clear();
        if stdin.lock().read_line(&mut line)? == 0 {
            eprintln!();
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (decls, rest) = take_prefix_decls(trimmed);
        for (name, iri) in decls {
            session.insert(name, iri);
        }
        if rest.is_empty() {
            let names: Vec<&str> = session.keys().map(String::as_str).collect();
            eprintln!("  session prefixes: {}\n", names.join(", "));
            continue;
        }
        let session_prologue: String = session
            .iter()
            .map(|(name, iri)| format!("PREFIX {name}: <{iri}>\n"))
            .collect();
        let effective = format!("{session_prologue}{rest}");
        match run_one_query(store, &effective, seed) {
            Ok(out) => println!("{out}\n"),
            Err(err) => eprintln!("query error: {err}\n"),
        }
    }
}

/// Peel leading `PREFIX name: <iri>` clauses off the front of a REPL line.
///
/// Returns the parsed `(name, iri)` pairs and the remaining query text. Parsing
/// stops at the first token that isn't a well-formed `PREFIX` clause, so an
/// inline prologue (`PREFIX e: <…> SELECT …`) and a bare declaration line
/// (`PREFIX e: <…>`) are both handled — the latter yields an empty remainder.
/// The `PREFIX` keyword match is case-insensitive per the SPARQL grammar.
#[cfg(feature = "oxigraph")]
fn take_prefix_decls(line: &str) -> (Vec<(String, String)>, &str) {
    let mut decls = Vec::new();
    let mut s = line;
    loop {
        let t = s.trim_start();
        match t.get(..6) {
            Some(kw) if kw.eq_ignore_ascii_case("PREFIX") => {}
            _ => return (decls, t),
        }
        let rest = &t[6..];
        let Some(colon) = rest.find(':') else {
            return (decls, t);
        };
        let after = &rest[colon + 1..];
        let Some(lt) = after.find('<') else {
            return (decls, t);
        };
        let Some(gt_rel) = after[lt + 1..].find('>') else {
            return (decls, t);
        };
        decls.push((
            rest[..colon].trim().to_string(),
            after[lt + 1..lt + 1 + gt_rel].to_string(),
        ));
        s = &after[lt + 1 + gt_rel + 1..];
    }
}

impl SeedArgs {
    /// Materialise the selection into a single [`SeedOntology`].
    ///
    /// `--seed-dir` without `--packs` merges every loaded pack; with
    /// `--packs` it filters by pack name (and errors if a requested name is
    /// missing). `clap` already rejects `--seed` + `--seed-dir` together and
    /// `--packs` without `--seed-dir`, so only the disk-resolution failures
    /// are handled here.
    fn resolve(&self) -> Result<Option<SeedOntology>, String> {
        if let Some(path) = &self.seed {
            return SeedOntology::load(path)
                .map(Some)
                .map_err(|e| format!("--seed {}: {e}", path.display()));
        }
        let Some(dir) = &self.seed_dir else {
            return Ok(None);
        };
        let loaded = SeedOntology::load_dir(dir)
            .map_err(|e| format!("--seed-dir {}: {e}", dir.display()))?;
        if loaded.is_empty() {
            return Err(format!("no .ttl files found in {}", dir.display()));
        }
        let selected = match &self.packs {
            None => loaded,
            Some(names) => {
                let available: Vec<String> = loaded.iter().map(|p| p.name.clone()).collect();
                for name in names {
                    if !available.iter().any(|a| a == name) {
                        return Err(format!(
                            "--packs: pack {name:?} not found in {} (available: {})",
                            dir.display(),
                            available.join(", "),
                        ));
                    }
                }
                loaded
                    .into_iter()
                    .filter(|p| names.iter().any(|n| n == &p.name))
                    .collect()
            }
        };
        let merged_name = match &self.packs {
            Some(names) => names.join("+"),
            None => "merged".to_owned(),
        };
        Ok(Some(merge_packs(merged_name, selected)?))
    }
}

/// Combine the selected packs into one [`SeedOntology`]. With the
/// `oxigraph` feature on we route through
/// [`SeedOntology::merged_via_oxigraph`] so each pack is parsed in its
/// own prefix context (handling cross-pack `@prefix` collisions and
/// keeping `owl:imports` triples intact); otherwise we fall back to the
/// regex-driven verbatim-concat path.
fn merge_packs(name: String, packs: Vec<SeedOntology>) -> Result<SeedOntology, String> {
    #[cfg(feature = "oxigraph")]
    {
        SeedOntology::merged_via_oxigraph(name, packs)
            .map_err(|e| format!("merge via oxigraph: {e}"))
    }
    #[cfg(not(feature = "oxigraph"))]
    {
        Ok(SeedOntology::merged(name, packs))
    }
}

/// Install the agent-side plugin(s) for the selected backend, attaching the
/// resolved seed, optional model override, and any alias seeds.
fn register_backend(
    server: &mut Server,
    backend: &Backend,
    seed: Option<SeedOntology>,
    model: Option<&str>,
    alias_seeds: &[(String, String)],
) {
    match backend {
        Backend::Memory => {
            let mut plugin = ExtractionPlugin::new();
            if let Some(s) = seed {
                plugin = plugin.with_seed(s);
            }
            if let Some(m) = model {
                plugin = plugin.with_model(m);
            }
            if !alias_seeds.is_empty() {
                plugin = plugin.with_aliases(alias_seeds.iter().cloned());
            }
            server.add_plugins(plugin);
        }
        #[cfg(feature = "oxigraph")]
        Backend::OxigraphMemory => {
            let mut plugin = OxigraphExtractionPlugin::new();
            if let Some(s) = seed {
                plugin = plugin.with_seed(s);
            }
            if let Some(m) = model {
                plugin = plugin.with_model(m);
            }
            if !alias_seeds.is_empty() {
                plugin = plugin.with_aliases(alias_seeds.iter().cloned());
            }
            server
                .add_plugins(OxigraphPlugin::in_memory())
                .add_plugins(plugin);
        }
        #[cfg(feature = "oxigraph-rocksdb")]
        Backend::OxigraphPath(path) => {
            let mut plugin = OxigraphExtractionPlugin::new();
            if let Some(s) = seed {
                plugin = plugin.with_seed(s);
            }
            if let Some(m) = model {
                plugin = plugin.with_model(m);
            }
            if !alias_seeds.is_empty() {
                plugin = plugin.with_aliases(alias_seeds.iter().cloned());
            }
            server
                .add_plugins(OxigraphPlugin::with_path(path.clone()))
                .add_plugins(plugin);
        }
    }
}

/// Branch the dump on backend: read the right resource and serialize the
/// Turtle snapshot. Then write the backend-agnostic markdown sidecar from
/// [`Sightings`].
fn snapshot(
    server: &Server,
    backend: &Backend,
    ttl_path: &Path,
    md_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let ctx = server.create_context();
    match backend {
        Backend::Memory => {
            let kg = ctx.get_resource::<KnowledgeGraph>()?;
            write_kg_dump(&kg, ttl_path)?;
            tracing::info!(triples = kg.len(), path = %ttl_path.display(), "KG snapshot");
        }
        #[cfg(feature = "oxigraph")]
        Backend::OxigraphMemory => {
            let store = ctx.get_resource::<OxigraphStore>()?;
            write_oxigraph_dump(&store, ttl_path)?;
            tracing::info!(quads = store.len()?, path = %ttl_path.display(), "oxigraph snapshot");
        }
        #[cfg(feature = "oxigraph-rocksdb")]
        Backend::OxigraphPath(_) => {
            let store = ctx.get_resource::<OxigraphStore>()?;
            write_oxigraph_dump(&store, ttl_path)?;
            tracing::info!(quads = store.len()?, path = %ttl_path.display(), "oxigraph snapshot");
        }
    }
    let sightings = ctx.get_resource::<Sightings>()?;
    write_pretty_dump(&sightings, md_path)?;
    tracing::info!(sightings = sightings.len(), path = %md_path.display(), "pretty snapshot");
    Ok(())
}

/// Resolve the `tail` source flags into a [`PrimarySource`].
///
/// Precedence (the variants are also `clap`-`conflicts_with`-guarded, so at
/// most one is set): `--stdin`, then `--file`, then a positional session id,
/// then the newest Claude Code session.
fn primary_source(args: &TailArgs) -> PrimarySource {
    if args.stdin {
        PrimarySource::Stdin
    } else if let Some(path) = &args.file {
        PrimarySource::File(path.clone())
    } else if let Some(id) = &args.session {
        PrimarySource::ClaudeSession(id.clone())
    } else {
        PrimarySource::ClaudeNewest
    }
}

/// The trace source the `tail` flow drives.
enum PrimarySource {
    Stdin,
    File(PathBuf),
    ClaudeSession(String),
    /// The no-arg default — picks the newest Claude Code session.
    ClaudeNewest,
}

/// KG backend selection. Defaults to [`Backend::OxigraphMemory`] when the
/// `oxigraph` feature is compiled in; otherwise falls back to
/// [`Backend::Memory`].
enum Backend {
    /// Naive in-memory append-only [`KnowledgeGraph`]. No feature flag required.
    Memory,
    /// In-memory oxigraph store split into ontology / data named graphs.
    #[cfg(feature = "oxigraph")]
    OxigraphMemory,
    /// RocksDB-backed oxigraph store at the given directory.
    #[cfg(feature = "oxigraph-rocksdb")]
    OxigraphPath(PathBuf),
}

/// Parse `--backend <kind>` into a [`Backend`].
///
/// Accepted forms:
/// - `oxigraph` — in-memory oxigraph store (default; requires `oxigraph` feature).
/// - `oxigraph:<dir>` — RocksDB-backed oxigraph store at `<dir>` (requires
///   `oxigraph-rocksdb` feature).
/// - `memory` — naive in-memory `KnowledgeGraph` (always available).
fn parse_backend(value: &str) -> Result<Backend, String> {
    let (kind, rest) = match value.split_once(':') {
        Some((k, r)) => (k, Some(r)),
        None => (value, None),
    };
    match (kind, rest) {
        ("memory", None) => Ok(Backend::Memory),
        ("memory", Some(_)) => Err("--backend memory takes no argument".to_string()),
        #[cfg(feature = "oxigraph")]
        ("oxigraph", None) => Ok(Backend::OxigraphMemory),
        #[cfg(feature = "oxigraph-rocksdb")]
        ("oxigraph", Some(path)) => Ok(Backend::OxigraphPath(PathBuf::from(path))),
        #[cfg(all(feature = "oxigraph", not(feature = "oxigraph-rocksdb")))]
        ("oxigraph", Some(_)) => {
            Err("oxigraph:<path> requires the `oxigraph-rocksdb` cargo feature".to_string())
        }
        #[cfg(not(feature = "oxigraph"))]
        ("oxigraph", _) => {
            Err("--backend oxigraph requires the `oxigraph` cargo feature".to_string())
        }
        (other, _) => Err(format!("unknown --backend value: {other}")),
    }
}

/// Wire the `tail` source plugin(s) onto `server`.
fn register_sources(server: &mut Server, args: &TailArgs) -> Result<(), String> {
    match primary_source(args) {
        PrimarySource::Stdin => {
            server.add_plugins(StdinIOPlugin::new());
        }
        PrimarySource::File(path) => {
            tracing::info!(path = %path.display(), "tailing file");
            server.add_plugins(FileTailIOPlugin::new(path));
        }
        PrimarySource::ClaudeSession(id) => {
            let plugin = ClaudeCodeIOPlugin::session(&id)?;
            tracing::info!(session = %id, "tailing Claude Code session");
            server.add_plugins(plugin);
        }
        PrimarySource::ClaudeNewest => {
            let plugin = ClaudeCodeIOPlugin::newest_session()?;
            tracing::info!("tailing newest Claude Code session");
            server.add_plugins(plugin);
        }
    }

    if let Some(bind) = args.otlp {
        server.add_plugins(OtlpIOPlugin::with_bind(bind));
    }

    Ok(())
}

/// Print known Claude Code sessions, newest first.
fn list_sessions() -> Result<(), Box<dyn std::error::Error>> {
    let rows = claude_code::list_sessions()?;
    if rows.is_empty() {
        let projects = claude_code::claude_projects_dir().unwrap_or_default();
        eprintln!("no sessions under {}", projects.display());
        return Ok(());
    }
    for info in rows {
        let stamp: chrono::DateTime<chrono::Utc> = info.mtime.into();
        println!(
            "{}  {}  {}",
            stamp.format("%Y-%m-%d %H:%M:%S"),
            info.id,
            info.project,
        );
    }
    Ok(())
}

#[cfg(all(test, feature = "oxigraph"))]
mod tests {
    use super::take_prefix_decls;

    #[test]
    fn bare_declaration_line_yields_empty_remainder() {
        let (decls, rest) = take_prefix_decls("PREFIX e: <urn:ontrace:e/>");
        assert_eq!(decls, vec![("e".into(), "urn:ontrace:e/".into())]);
        assert_eq!(rest, "");
    }

    #[test]
    fn inline_prologue_keeps_the_query_body() {
        let (decls, rest) =
            take_prefix_decls("PREFIX e: <urn:ontrace:e/> SELECT ?p WHERE { e:alice ?p ?o }");
        assert_eq!(decls, vec![("e".into(), "urn:ontrace:e/".into())]);
        assert_eq!(rest, "SELECT ?p WHERE { e:alice ?p ?o }");
    }

    #[test]
    fn multiple_clauses_and_case_insensitive_keyword() {
        let (decls, rest) =
            take_prefix_decls("prefix e: <urn:ontrace:e/> Prefix p: <urn:ontrace:p/> ASK {}");
        assert_eq!(
            decls,
            vec![
                ("e".into(), "urn:ontrace:e/".into()),
                ("p".into(), "urn:ontrace:p/".into()),
            ],
        );
        assert_eq!(rest, "ASK {}");
    }

    #[test]
    fn query_without_prefix_passes_through_untouched() {
        let (decls, rest) = take_prefix_decls("SELECT * WHERE { ?s ?p ?o }");
        assert!(decls.is_empty());
        assert_eq!(rest, "SELECT * WHERE { ?s ?p ?o }");
    }

    #[test]
    fn default_prefix_has_empty_name() {
        let (decls, rest) = take_prefix_decls("PREFIX : <urn:ontrace:e/>");
        assert_eq!(decls, vec![(String::new(), "urn:ontrace:e/".into())]);
        assert_eq!(rest, "");
    }
}

/// Write the in-memory KG to `path` atomically: serialize into `<path>.tmp`
/// first, then rename. Guarantees a concurrent reader never sees a
/// half-written file.
fn write_kg_dump(kg: &KnowledgeGraph, path: &Path) -> std::io::Result<()> {
    let tmp = path.with_extension("ttl.tmp");
    {
        let file = std::fs::File::create(&tmp)?;
        let mut writer = std::io::BufWriter::new(file);
        dump::write_turtle(kg, &mut writer)?;
        std::io::Write::flush(&mut writer)?;
    }
    std::fs::rename(&tmp, path)
}

/// Write the markdown extraction report to `path` atomically.
///
/// Mirrors [`write_kg_dump`] / [`write_oxigraph_dump`]: serialize into a
/// `<path>.tmp` sibling first, then rename — a concurrent reader never
/// sees a half-written sidecar.
fn write_pretty_dump(sightings: &Sightings, path: &Path) -> std::io::Result<()> {
    let tmp = path.with_extension("md.tmp");
    {
        let file = std::fs::File::create(&tmp)?;
        let mut writer = std::io::BufWriter::new(file);
        dump::write_pretty(sightings, &mut writer)?;
        std::io::Write::flush(&mut writer)?;
    }
    std::fs::rename(&tmp, path)
}

/// Write the oxigraph data (`ABox`) graph to `path` as Turtle, atomically.
///
/// Only the data graph is included — the ontology (`TBox`) is curated
/// content that rarely changes and doesn't need to be in every snapshot.
/// Dump it separately when needed via the `dump_turtle` API on the store.
#[cfg(feature = "oxigraph")]
fn write_oxigraph_dump(
    store: &OxigraphStore,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let ttl = store.dump_turtle(store.data_graph())?;
    let tmp = path.with_extension("ttl.tmp");
    std::fs::write(&tmp, ttl)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
