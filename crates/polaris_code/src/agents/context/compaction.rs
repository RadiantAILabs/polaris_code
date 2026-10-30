//! The compaction capability's command, `/compact`.
//!
//! The graph already summarizes on its own when the window grows past a
//! threshold (the graph's `compact_context` node). `/compact` lets the user ask
//! for that checkpoint now, with a focus. It is contributed by the
//! capability that implements it, not by the generic command contract, and only
//! alongside the agent that reads it: see [`CompactionPlugin`].

use std::sync::Arc;

use polaris::plugins::IOMessage;
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris_tui::commands::CommandSpec;

use crate::agents::code_agent::CodeAgentPlugin;
use crate::terminal::commands::{AgentRequest, CommandError, CommandOutcome, CommandRegistry};

/// The kind of the [`AgentRequest`] that `/compact` makes.
const COMPACT_REQUEST: &str = "polaris-code:compact";

/// The focus text of a `/compact` request, or `None` for any other message.
///
/// Read by the agent's input system, the one consumer of the request.
pub(crate) fn compaction_focus(message: &IOMessage) -> Option<String> {
    AgentRequest::from_message(message)
        .filter(|request| request.kind() == COMPACT_REQUEST)
        .map(|request| request.payload().to_owned())
}

/// Register `/compact [focus text]` in `commands`.
fn register(commands: &CommandRegistry) -> Result<(), CommandError> {
    commands.register_sync(
        CommandSpec::new("compact", "Summarize older history into a checkpoint")
            .with_usage("[focus text]"),
        |_, args| CommandOutcome::Request(AgentRequest::new(COMPACT_REQUEST, args)),
    )
}

/// Adds `/compact`, which asks the agent graph for a summary checkpoint now.
///
/// Add it next to [`CodeAgentPlugin`] when the terminal should offer that
/// command. The command carries a typed [`AgentRequest`] to the agent's input
/// system, which asks the graph's metered summary node for a checkpoint
/// instead of a normal answer. Canonical history is never changed, and the
/// request is not added to it.
///
/// The plugin exists so that the command is installed only where something
/// reads it. It requires [`CodeAgentPlugin`], so assembling it without the
/// agent fails at startup, and a host that runs a different agent, or none,
/// leaves it out and never advertises a command it cannot serve. Without it
/// the agent still summarizes automatically past its threshold.
///
/// # Resources Provided
///
/// None. It registers a command and carries no state of its own.
///
/// # Dependencies
///
/// Requires [`CodeAgentPlugin`], the consumer of the request it makes.
///
/// # Lifecycle
///
/// `build` registers `/compact` in the supplied [`CommandRegistry`]. If a
/// command of that name already exists it is left in place and the refusal is
/// logged, because the agent works without the command. It starts no tasks.
///
/// # Example
///
/// ```no_run
/// use std::sync::Arc;
///
/// use polaris::models::ModelsPlugin;
/// use polaris::plugins::PersistencePlugin;
/// use polaris::system::server::Server;
/// use polaris::tools::ToolsPlugin;
/// use polaris_code::agents::CodeAgentPlugin;
/// use polaris_code::agents::context::CompactionPlugin;
/// use polaris_code::terminal::CommandRegistry;
///
/// # async fn assemble() {
/// let commands = Arc::new(CommandRegistry::with_builtins());
/// let mut server = Server::new();
/// server
///     .add_plugins(ModelsPlugin)
///     .add_plugins(ToolsPlugin)
///     .add_plugins(PersistencePlugin)
///     .add_plugins(CodeAgentPlugin::new())
///     .add_plugins(CompactionPlugin::new(commands.clone()));
/// server.finish().await.expect("the agent and its command assemble");
/// assert!(commands.contains("compact"));
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct CompactionPlugin {
    commands: Arc<CommandRegistry>,
}

impl CompactionPlugin {
    /// Contribute `/compact` to `commands`.
    #[must_use]
    pub fn new(commands: Arc<CommandRegistry>) -> Self {
        Self { commands }
    }
}

impl Plugin for CompactionPlugin {
    const ID: &'static str = "polaris_code::compaction";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<CodeAgentPlugin>()]
    }

    fn build(&self, _server: &mut Server) {
        if let Err(error) = register(&self.commands) {
            tracing::warn!(%error, "the `/compact` command was not registered");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_tui::commands::CommandInvocation;

    fn compact(args: &str) -> CommandInvocation {
        CommandInvocation {
            name: "compact".into(),
            args: args.into(),
        }
    }

    #[tokio::test]
    async fn the_command_asks_the_agent_for_a_summary_and_keeps_the_focus() {
        let registry = CommandRegistry::new();
        register(&registry).unwrap();

        let outcome = registry
            .dispatch(&compact("preserve the test failures"))
            .await;
        let CommandOutcome::Request(request) = &outcome else {
            panic!("`/compact` makes a request of the agent, not a reply: {outcome:?}");
        };
        assert_eq!(request.kind(), COMPACT_REQUEST);

        let message = outcome.into_message("compact");
        assert_eq!(
            compaction_focus(&message).as_deref(),
            Some("preserve the test failures")
        );
        // Typed as a prompt, the same words are only text.
        let typed = IOMessage::user_text("/compact preserve the test failures");
        assert!(compaction_focus(&typed).is_none());
    }

    #[test]
    fn only_a_compaction_request_is_read_as_one() {
        let other = AgentRequest::new("test:other", "focus").into_message();
        assert!(compaction_focus(&other).is_none());
        let ours = AgentRequest::new(COMPACT_REQUEST, "").into_message();
        assert_eq!(compaction_focus(&ours).as_deref(), Some(""));
    }

    #[test]
    fn registering_twice_is_refused_and_does_not_replace_the_command() {
        let registry = CommandRegistry::new();
        register(&registry).unwrap();
        assert_eq!(
            register(&registry),
            Err(CommandError::Duplicate("compact".into()))
        );
        assert_eq!(registry.names(), ["compact"]);
    }
}
