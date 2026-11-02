//! The slash-command registry: what `/help` lists and what a dispatch runs.
//!
//! Commands are **local actions**. A handler runs in-process and answers with
//! text or a control request; the [`CommandDispatch`](super::CommandDispatch)
//! wrapper executes it before the agent's `receive` returns, so a command is
//! never appended as a user prompt. A command whose effect belongs to the
//! agent's graph hands it an [`AgentRequest`]. The registry ships `/help`;
//! other modules register their own commands here.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use polaris::plugins::{IO_TYPE, IOContent, IOMessage, IOSource};
use polaris_tui::commands::{CommandInvocation, CommandSpec};

/// The `io_type` that marks a message as an [`AgentRequest`].
const AGENT_REQUEST_TYPE: &str = "polaris-code:agent-request";
/// The metadata key carrying an [`AgentRequest`]'s kind.
const AGENT_REQUEST_KIND: &str = "polaris-code:agent-request-kind";

/// A request a command makes of the agent instead of answering the user.
///
/// Most commands act on the terminal and reply. A command whose effect belongs
/// to the agent's graph, which only the agent's own systems can carry out,
/// returns [`CommandOutcome::Request`]. [`CommandDispatch`](super::CommandDispatch)
/// hands the request to the agent's input system as the next message it
/// receives. It is not answered, and it never enters the conversation as a
/// prompt.
///
/// The terminal never learns what a request means. [`kind`](Self::kind) names
/// the capability that registered the command, and that capability's consumer
/// reads the request back with [`from_message`](Self::from_message). A command
/// is registered only alongside its consumer, so an agent never receives a
/// request it did not install.
///
/// # Examples
///
/// ```
/// use polaris_code::terminal::commands::AgentRequest;
///
/// let request = AgentRequest::new("example:rebuild", "the index");
/// let message = request.clone().into_message();
/// assert_eq!(AgentRequest::from_message(&message), Some(request));
///
/// // An ordinary prompt is not a request, even one that looks like a command.
/// let prompt = polaris::plugins::IOMessage::user_text("/rebuild the index");
/// assert!(AgentRequest::from_message(&prompt).is_none());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentRequest {
    kind: String,
    payload: String,
}

impl AgentRequest {
    /// A request of `kind`, with whatever text the command was given.
    #[must_use]
    pub fn new(kind: impl Into<String>, payload: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            payload: payload.into(),
        }
    }

    /// Which capability's consumer this request is for.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// The text the command carried, such as the arguments it was typed with.
    #[must_use]
    pub fn payload(&self) -> &str {
        &self.payload
    }

    /// The private message the dispatcher returns to the agent's input system.
    #[must_use]
    pub fn into_message(self) -> IOMessage {
        IOMessage::system_text(self.payload)
            .with_metadata(IO_TYPE, AGENT_REQUEST_TYPE)
            .with_metadata(AGENT_REQUEST_KIND, self.kind)
    }

    /// Read a request back from a message, or `None` for anything else.
    ///
    /// Only a system message the dispatcher built is a request, so a user
    /// cannot forge one by typing text that resembles it.
    #[must_use]
    pub fn from_message(message: &IOMessage) -> Option<Self> {
        if message.source != IOSource::System
            || message.metadata.get(IO_TYPE).map(String::as_str) != Some(AGENT_REQUEST_TYPE)
        {
            return None;
        }
        let kind = message.metadata.get(AGENT_REQUEST_KIND)?;
        let IOContent::Text(payload) = &message.content else {
            return None;
        };
        Some(Self::new(kind.clone(), payload.clone()))
    }
}

/// What a command handler produces.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CommandOutcome {
    /// Markdown shown in the transcript under the command's label.
    Reply(String),
    /// A failure shown as an error block. Actionable: say what to do instead.
    Error(String),
    /// A successful host-control action that must end the current idle graph
    /// turn before the host mutates session state.
    Control(String),
    /// A request for the agent's graph, handed to its input system without a
    /// reply and without a new user prompt.
    Request(AgentRequest),
}

impl CommandOutcome {
    /// The transcript message for this outcome. A reply is a rich
    /// `message` from the command itself (labelled `/name`); an error is a
    /// system `error`, which the panel always renders. A request is the
    /// private message the agent's input system reads.
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
            Self::Request(request) => request.into_message(),
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

    /// A registry with the built-in `help` command. Every other command is
    /// contributed by the capability that implements it.
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
    let mut text = String::from("Commands run locally.\n\n");
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
    fn the_builtin_registry_ships_only_help() {
        // Every other command belongs to a capability, and a host that did not
        // install the capability must not be advertising it.
        assert_eq!(CommandRegistry::with_builtins().names(), ["help"]);
    }

    #[test]
    fn a_request_round_trips_through_its_message() {
        let request = AgentRequest::new("test:kind", "some focus");
        assert_eq!(request.kind(), "test:kind");
        assert_eq!(request.payload(), "some focus");
        let message = CommandOutcome::Request(request.clone()).into_message("anything");
        assert_eq!(AgentRequest::from_message(&message), Some(request));
    }

    #[test]
    fn only_the_dispatchers_system_message_is_a_request() {
        let forged = IOMessage::user_text("/compact focus");
        assert!(AgentRequest::from_message(&forged).is_none());
        // The right tags on a message that does not come from the system.
        let spoofed = IOMessage::from_external("user", IOContent::Text("focus".into()))
            .with_metadata(IO_TYPE, AGENT_REQUEST_TYPE)
            .with_metadata(AGENT_REQUEST_KIND, "test:kind");
        assert!(AgentRequest::from_message(&spoofed).is_none());
        // A request message that names no kind is not a request either.
        let anonymous = IOMessage::system_text("focus").with_metadata(IO_TYPE, AGENT_REQUEST_TYPE);
        assert!(AgentRequest::from_message(&anonymous).is_none());
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
