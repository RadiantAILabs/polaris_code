//! The slash-command registry: what `/help` lists and what a dispatch runs.
//!
//! Commands are **local actions**. A handler runs in-process, answers with
//! text, and never touches the model; the [`CommandDispatch`](super::CommandDispatch)
//! wrapper executes it before the agent's `receive` returns, so a command is
//! not a turn. Other modules register their own commands here; this module
//! ships only `help`.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use polaris::plugins::{IO_TYPE, IOContent, IOMessage};
use polaris_tui::commands::{CommandInvocation, CommandSpec};

/// What a command handler produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    /// Markdown shown in the transcript under the command's label.
    Reply(String),
    /// A failure shown as an error block. Actionable: say what to do instead.
    Error(String),
    /// A successful host-control action that must end the current idle graph
    /// turn before the host mutates session state.
    Control(String),
}

impl CommandOutcome {
    /// The transcript message for this outcome. A reply is a rich
    /// `message` from the command itself (labelled `/name`); an error is a
    /// system `error`, which the panel always renders.
    #[must_use]
    pub fn into_message(self, name: &str) -> IOMessage {
        match self {
            Self::Reply(text) => {
                IOMessage::from_external(format!("/{name}"), IOContent::Text(text))
                    .with_metadata(IO_TYPE, "message")
            }
            Self::Error(text) => IOMessage::system_text(text).with_metadata(IO_TYPE, "error"),
            Self::Control(text) => {
                IOMessage::from_external(format!("/{name}"), IOContent::Text(text))
                    .with_metadata(IO_TYPE, "message")
            }
        }
    }

    /// Whether dispatch must release the current session's context lock.
    #[must_use]
    pub fn ends_turn(&self) -> bool {
        matches!(self, Self::Control(_))
    }
}

/// What a handler sees: the registry (so `help` can list it) and the
/// argument text after the command name.
pub struct CommandContext<'a> {
    /// The registry the command was dispatched from.
    pub registry: &'a CommandRegistry,
    /// Everything after the name, trimmed.
    pub args: &'a str,
}

/// Future a handler returns.
pub type CommandFuture<'a> = Pin<Box<dyn Future<Output = CommandOutcome> + Send + 'a>>;

/// A command's implementation.
pub type CommandHandler =
    Arc<dyn for<'a> Fn(CommandContext<'a>) -> CommandFuture<'a> + Send + Sync + 'static>;

struct Registered {
    spec: CommandSpec,
    handler: CommandHandler,
}

/// Why a registration was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandError {
    /// A command with this name already exists.
    Duplicate(String),
    /// The name is empty or contains whitespace and could never be typed.
    InvalidName(String),
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicate(name) => write!(f, "command `/{name}` is already registered"),
            Self::InvalidName(name) => write!(f, "`{name:?}` is not a valid command name"),
        }
    }
}

impl std::error::Error for CommandError {}

/// The set of `/commands` the terminal executes locally.
///
/// Cheap to share (`Arc` it) and safe to register into from several plugins:
/// registration is `&self`. Names are case-insensitive and stored lower-case.
#[derive(Default)]
pub struct CommandRegistry {
    commands: RwLock<BTreeMap<String, Registered>>,
}

impl fmt::Debug for CommandRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandRegistry")
            .field("commands", &self.names())
            .finish()
    }
}

impl CommandRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry with the built-in `help` command.
    #[must_use]
    pub fn with_builtins() -> Self {
        let registry = Self::new();
        registry
            .register_sync(
                CommandSpec::new("help", "List the available commands").with_usage("[command]"),
                help,
            )
            .expect("fresh registry has no `help` yet");
        registry
    }

    /// Register an async command.
    ///
    /// # Errors
    ///
    /// Refuses a name that is empty, contains whitespace, or is taken.
    pub fn register(&self, spec: CommandSpec, handler: CommandHandler) -> Result<(), CommandError> {
        let name = spec.name.trim().to_lowercase();
        if name.is_empty() || name.chars().any(char::is_whitespace) {
            return Err(CommandError::InvalidName(spec.name));
        }
        let mut commands = self.commands.write().unwrap_or_else(|e| e.into_inner());
        if commands.contains_key(&name) {
            return Err(CommandError::Duplicate(name));
        }
        let spec = CommandSpec {
            name: name.clone(),
            ..spec
        };
        commands.insert(name, Registered { spec, handler });
        Ok(())
    }

    /// Register a synchronous command.
    ///
    /// # Errors
    ///
    /// As [`register`](Self::register).
    pub fn register_sync<F>(&self, spec: CommandSpec, handler: F) -> Result<(), CommandError>
    where
        F: Fn(&CommandRegistry, &str) -> CommandOutcome + Send + Sync + 'static,
    {
        let handler = Arc::new(handler);
        self.register(
            spec,
            Arc::new(move |ctx: CommandContext<'_>| {
                let outcome = handler(ctx.registry, ctx.args);
                Box::pin(async move { outcome }) as CommandFuture<'_>
            }),
        )
    }

    /// The advertised specs, in name order.
    #[must_use]
    pub fn specs(&self) -> Vec<CommandSpec> {
        self.commands
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|registered| registered.spec.clone())
            .collect()
    }

    /// The registered names, in order.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.commands
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    /// Whether `name` is registered.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.commands
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&name.to_lowercase())
    }

    /// Run `invocation`. An unknown name is reported as an error that names
    /// the closest registered commands and points at `/help`, so a typo is
    /// a correction away rather than a silent no-op — or a model request.
    pub async fn dispatch(&self, invocation: &CommandInvocation) -> CommandOutcome {
        let name = invocation.name.to_lowercase();
        let handler = self
            .commands
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&name)
            .map(|registered| registered.handler.clone());
        match handler {
            Some(handler) => {
                handler(CommandContext {
                    registry: self,
                    args: &invocation.args,
                })
                .await
            }
            None => CommandOutcome::Error(self.unknown(&name)),
        }
    }

    /// The error text for an unknown command.
    fn unknown(&self, name: &str) -> String {
        let similar: Vec<String> = self
            .names()
            .into_iter()
            .filter(|known| known.starts_with(name) || name.starts_with(known.as_str()))
            .map(|known| format!("/{known}"))
            .collect();
        let mut text = format!("unknown command `/{name}`");
        if !similar.is_empty() {
            text.push_str(&format!(" — did you mean {}?", similar.join(", ")));
        }
        text.push_str(" Type /help to list the available commands.");
        text
    }
}

/// `/help [command]`: list every command, or describe one.
fn help(registry: &CommandRegistry, args: &str) -> CommandOutcome {
    let specs = registry.specs();
    let wanted = args.trim().trim_start_matches('/').to_lowercase();
    if !wanted.is_empty() {
        return match specs.iter().find(|spec| spec.name == wanted) {
            Some(spec) => CommandOutcome::Reply(describe(spec)),
            None => CommandOutcome::Error(registry.unknown(&wanted)),
        };
    }
    let width = specs
        .iter()
        .map(|spec| usage_column(spec).len())
        .max()
        .unwrap_or(0);
    let mut text = String::from("Commands run locally and never reach the model.\n\n");
    for spec in &specs {
        let usage = usage_column(spec);
        text.push_str(&format!(
            "- `{usage}`{}  {}\n",
            " ".repeat(width - usage.len()),
            spec.description
        ));
    }
    text.push_str(
        "\nType `/` to search commands, `@` to mention a workspace file, and Tab to accept.",
    );
    CommandOutcome::Reply(text)
}

fn usage_column(spec: &CommandSpec) -> String {
    match &spec.usage {
        Some(usage) => format!("/{} {usage}", spec.name),
        None => format!("/{}", spec.name),
    }
}

fn describe(spec: &CommandSpec) -> String {
    format!("`{}` — {}", usage_column(spec), spec.description)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn help_lists_every_command_and_describes_one() {
        let registry = CommandRegistry::with_builtins();
        registry
            .register_sync(
                CommandSpec::new("model", "Pick a model").with_usage("<id>"),
                |_, args| CommandOutcome::Reply(format!("model {args}")),
            )
            .unwrap();
        let CommandOutcome::Reply(text) =
            registry.dispatch(&CommandInvocation::new("help", "")).await
        else {
            panic!("help replies")
        };
        assert!(text.contains("/help [command]"), "{text}");
        assert!(text.contains("/model <id>"), "{text}");
        assert!(text.contains("Pick a model"), "{text}");

        let CommandOutcome::Reply(text) = registry
            .dispatch(&CommandInvocation::new("help", "/model"))
            .await
        else {
            panic!("help on a command replies")
        };
        assert_eq!(text, "`/model <id>` — Pick a model");

        let CommandOutcome::Error(text) = registry
            .dispatch(&CommandInvocation::new("help", "nope"))
            .await
        else {
            panic!("help on an unknown command errors")
        };
        assert!(text.contains("unknown command `/nope`"), "{text}");
    }

    #[tokio::test]
    async fn unknown_commands_are_reported_with_suggestions() {
        let registry = CommandRegistry::with_builtins();
        let CommandOutcome::Error(text) =
            registry.dispatch(&CommandInvocation::new("hel", "")).await
        else {
            panic!("unknown errors")
        };
        assert!(text.contains("did you mean /help"), "{text}");
        assert!(text.contains("/help"), "{text}");

        let CommandOutcome::Error(text) =
            registry.dispatch(&CommandInvocation::new("zzz", "")).await
        else {
            panic!("unknown errors")
        };
        assert!(!text.contains("did you mean"), "{text}");
        assert!(text.contains("Type /help"), "{text}");
    }

    #[tokio::test]
    async fn registration_rejects_duplicates_and_bad_names_and_ignores_case() {
        let registry = CommandRegistry::with_builtins();
        assert_eq!(
            registry.register_sync(CommandSpec::new("HELP", "again"), |_, _| {
                CommandOutcome::Reply(String::new())
            }),
            Err(CommandError::Duplicate("help".into()))
        );
        assert_eq!(
            registry.register_sync(CommandSpec::new("two words", "x"), |_, _| {
                CommandOutcome::Reply(String::new())
            }),
            Err(CommandError::InvalidName("two words".into()))
        );
        assert!(registry.contains("Help"));
        let outcome = registry.dispatch(&CommandInvocation::new("HELP", "")).await;
        assert!(matches!(outcome, CommandOutcome::Reply(_)));
    }

    #[tokio::test]
    async fn async_handlers_receive_their_args() {
        let registry = CommandRegistry::new();
        registry
            .register(
                CommandSpec::new("echo", "Echo"),
                Arc::new(|ctx: CommandContext<'_>| {
                    let args = ctx.args.to_string();
                    Box::pin(async move { CommandOutcome::Reply(args) }) as CommandFuture<'_>
                }),
            )
            .unwrap();
        assert_eq!(
            registry
                .dispatch(&CommandInvocation::new("echo", "hi there"))
                .await,
            CommandOutcome::Reply("hi there".into())
        );
    }

    #[test]
    fn outcomes_become_the_expected_message_kinds() {
        let reply = CommandOutcome::Reply("ok".into()).into_message("help");
        assert_eq!(reply.metadata.get(IO_TYPE).unwrap(), "message");
        assert!(matches!(&reply.source, polaris::plugins::IOSource::External(n) if n == "/help"));
        let error = CommandOutcome::Error("bad".into()).into_message("help");
        assert_eq!(error.metadata.get(IO_TYPE).unwrap(), "error");
    }
}
