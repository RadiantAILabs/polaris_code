//! Independently composed rule mechanisms and conversation budget.

use super::record::ModelCall;
use super::state::EndInput;
use super::{RunRules, StepState};
use polaris::system::plugin::{Contract, Plugin, PluginAccess, PluginId, Version, VersionReq};
use polaris::system::server::Server;
use polaris_code::agents::observation::{ModelObserver, TurnObserver};
use polaris_code::agents::permission_rules::{
    PermissionRules, PermissionRulesPlugin, RuleDecision,
};
use polaris_code::agents::{AgentConfig, CodeAgentPlugin, CodeAgentSteps, ContextManager};

pub(crate) fn install(server: &mut Server, rules: RunRules) {
    if !rules.is_active() {
        return;
    }
    server.add_plugins(RulesStatePlugin(rules.clone()));
    server.add_plugins(RunRecordPlugin(rules.clone()));
    if rules.prompt_section().is_some() {
        server.add_plugins(PromptRulesPlugin(rules.clone()));
    }
    if rules.guard().is_some() {
        server.add_plugins(PerCallRulesPlugin(rules.clone()));
    }
    if rules.steps_enabled() {
        server.add_plugins(RuleStepsPlugin);
    }
    if let Some(maximum) = rules.max_messages() {
        server.add_plugins(MessageLimitPlugin::new(maximum).with_record(rules));
    }
}

/// Provides the immutable protected-file seal and per-session gate state; add it when assembling rule mechanisms.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`RunRules`] | Global | One run's immutable configuration, seal and recorder. |
/// | [`StepState`] | Local | Gate holds, fingerprint and findings within each session. |
///
/// # APIs Provided
///
/// None; this plugin composes stock resources and registries.
///
/// # Dependencies
///
/// None.
///
/// # Lifecycle
///
/// Contributions are registered during `build` and frozen with stock registries.
/// No worker tasks are spawned. Session setup runs on creation and resume.
///
/// # Example
///
/// ```no_run
/// use polaris::system::server::Server;
/// use polaris_code::agents::CodeAgentPlugin;
/// use polaris_guards::rules::{Arm, RulesConfig, RunRules};
/// use polaris_guards::rules::plugins::*;
/// let rules = RunRules::new(RulesConfig::new(Arm::Prompt), std::path::Path::new("."))?;
/// let mut server = Server::new();
/// // Add the stock models, tools and sessions before the coding agent.
/// server.add_plugins(CodeAgentPlugin::new());
/// server.add_plugins(RulesStatePlugin(RunRules::inactive()));
/// # Ok::<(), String>(())
/// ```
#[derive(Clone)]
pub struct RulesStatePlugin(pub RunRules);
impl Plugin for RulesStatePlugin {
    const ID: &'static str = "polaris_guards::state";
    const VERSION: Version = Version::new(0, 1, 0);
    fn build(&self, server: &mut Server) {
        server.insert_global(self.0.clone());
        let root = self.0.root();
        server.register_local(move || StepState::new(&root));
    }
}

/// Appends configured rule words after stock system-prompt grounding; use it for prompt-bearing arms.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_ | — | Extends the existing session configuration. |
///
/// # APIs Provided
///
/// None; this plugin composes stock resources and registries.
///
/// # Dependencies
///
/// [`CodeAgentPlugin`]. Mutates `AgentConfig` only through session setup.
///
/// # Lifecycle
///
/// Contributions are registered during `build` and frozen with stock registries.
/// No worker tasks are spawned. Session setup runs on creation and resume.
///
/// # Example
///
/// ```no_run
/// use polaris::system::server::Server;
/// use polaris_code::agents::CodeAgentPlugin;
/// use polaris_guards::rules::{Arm, RulesConfig, RunRules};
/// use polaris_guards::rules::plugins::*;
/// let rules = RunRules::new(RulesConfig::new(Arm::Prompt), std::path::Path::new("."))?;
/// let mut server = Server::new();
/// // Add the stock models, tools and sessions before the coding agent.
/// server.add_plugins(CodeAgentPlugin::new());
/// server.add_plugins(PromptRulesPlugin(rules));
/// # Ok::<(), String>(())
/// ```
pub struct PromptRulesPlugin(pub RunRules);
impl Plugin for PromptRulesPlugin {
    const ID: &'static str = "polaris_guards::prompt";
    const VERSION: Version = Version::new(0, 1, 0);
    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<CodeAgentPlugin>()]
    }
    fn build(&self, server: &mut Server) {
        let section = self.0.prompt_section().expect("prompt arm");
        server
            .get_resource_mut::<CodeAgentSteps>()
            .unwrap()
            .configure_session("rules.prompt", move |ctx| {
                ctx.get_resource_mut::<AgentConfig>()
                    .map_err(polaris::agent::SetupError::new)?
                    .prompt_sections
                    .push(section.clone());
                Ok(())
            })
            .unwrap();
    }
}

/// Adds an integrity code permission rule; use it to refuse guarded calls through the ordinary permission policy.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_ | — | Extends the shared permission and step registries. |
///
/// # APIs Provided
///
/// None; this plugin composes stock resources and registries.
///
/// # Dependencies
///
/// [`PermissionRulesPlugin`], [`CodeAgentPlugin`] and [`RulesStatePlugin`]. The rule returns deterministic decisions; its metadata cache is observational only.
///
/// # Lifecycle
///
/// Contributions are registered during `build` and frozen with stock registries.
/// No worker tasks are spawned. Session setup runs on creation and resume.
///
/// # Example
///
/// ```no_run
/// use polaris::system::server::Server;
/// use polaris_code::agents::CodeAgentPlugin;
/// use polaris_guards::rules::{Arm, RulesConfig, RunRules};
/// use polaris_guards::rules::plugins::*;
/// let rules = RunRules::new(RulesConfig::new(Arm::Prompt), std::path::Path::new("."))?;
/// let mut server = Server::new();
/// // Add the stock models, tools and sessions before the coding agent.
/// server.add_plugins(polaris_code::agents::permission_rules::PermissionRulesPlugin::new("."));
/// server.add_plugins(CodeAgentPlugin::new());
/// server.add_plugins(RulesStatePlugin(rules.clone()));
/// server.add_plugins(PerCallRulesPlugin(rules));
/// # Ok::<(), String>(())
/// ```
pub struct PerCallRulesPlugin(pub RunRules);
impl Plugin for PerCallRulesPlugin {
    const ID: &'static str = "polaris_guards::per_call";
    const VERSION: Version = Version::new(0, 1, 0);
    fn dependencies(&self) -> Vec<PluginId> {
        vec![
            PluginId::of::<PermissionRulesPlugin>(),
            PluginId::of::<CodeAgentPlugin>(),
            PluginId::of::<RulesStatePlugin>(),
        ]
    }
    fn access(&self) -> PluginAccess {
        PluginAccess::new()
            .extends::<PermissionRules>(VersionReq::caret(PermissionRules::CONTRACT_VERSION))
    }
    fn build(&self, server: &mut Server) {
        let rules = self.0.clone();
        server
            .get_resource_mut::<PermissionRules>()
            .unwrap()
            .add_code_rule("rules.integrity", move |tool, args, _root| {
                rules.guard()?.check(tool, args).map(|refusal| {
                    let message = rules.refusal_message(&refusal);
                    rules.remember_refusal(message.clone(), refusal);
                    (RuleDecision::Deny, message)
                })
            })
            .unwrap();
        server
            .get_resource_mut::<CodeAgentSteps>()
            .unwrap()
            .after_tools("rules.refusals", -100, |graph| {
                graph.add_system(super::steps::record_refusals);
            })
            .unwrap();
    }
}

/// Adds restoration, tree observation and a sealed completion gate; use it for graph-enforced arms.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_ | — | Reads `RunRules` and mutates session-local `StepState`. |
///
/// # APIs Provided
///
/// None; this plugin composes stock resources and registries.
///
/// # Dependencies
///
/// [`CodeAgentPlugin`] and [`RulesStatePlugin`].
///
/// # Lifecycle
///
/// Contributions are registered during `build` and frozen with stock registries.
/// No worker tasks are spawned. Session setup runs on creation and resume.
///
/// # Example
///
/// ```no_run
/// use polaris::system::server::Server;
/// use polaris_code::agents::CodeAgentPlugin;
/// use polaris_guards::rules::{Arm, RulesConfig, RunRules};
/// use polaris_guards::rules::plugins::*;
/// let rules = RunRules::new(RulesConfig::new(Arm::Prompt), std::path::Path::new("."))?;
/// let mut server = Server::new();
/// // Add the stock models, tools and sessions before the coding agent.
/// server.add_plugins(CodeAgentPlugin::new());
/// server.add_plugins(RuleStepsPlugin);
/// # Ok::<(), String>(())
/// ```
pub struct RuleStepsPlugin;
impl Plugin for RuleStepsPlugin {
    const ID: &'static str = "polaris_guards::steps";
    const VERSION: Version = Version::new(0, 1, 0);
    fn dependencies(&self) -> Vec<PluginId> {
        vec![
            PluginId::of::<CodeAgentPlugin>(),
            PluginId::of::<RulesStatePlugin>(),
        ]
    }
    fn build(&self, server: &mut Server) {
        let mut steps = server.get_resource_mut::<CodeAgentSteps>().unwrap();
        steps
            .after_tools("rules.observe", 0, |graph| {
                graph.add_system(super::steps::observe_step);
            })
            .unwrap();
        steps
            .before_finish("rules.gate", 0, |graph| {
                graph.add_system(super::steps::finish_gate);
            })
            .unwrap();
    }
}

/// Bounds canonical conversation messages; use it with any arm or by itself.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_ | — | Configures the existing `ContextManager` after session setup. |
///
/// # APIs Provided
///
/// None; this plugin composes stock resources and registries.
///
/// # Dependencies
///
/// [`CodeAgentPlugin`]. No rule arm or rule-state plugin is required.
///
/// # Lifecycle
///
/// Contributions are registered during `build` and frozen with stock registries.
/// No worker tasks are spawned. Session setup runs on creation and resume.
///
/// # Example
///
/// ```no_run
/// use polaris::system::server::Server;
/// use polaris_code::agents::CodeAgentPlugin;
/// use polaris_guards::rules::{Arm, RulesConfig, RunRules};
/// use polaris_guards::rules::plugins::*;
/// let rules = RunRules::new(RulesConfig::new(Arm::Prompt), std::path::Path::new("."))?;
/// let mut server = Server::new();
/// // Add the stock models, tools and sessions before the coding agent.
/// server.add_plugins(CodeAgentPlugin::new());
/// server.add_plugins(MessageLimitPlugin::new(100));
/// # Ok::<(), String>(())
/// ```
#[derive(Clone)]
pub struct MessageLimitPlugin {
    maximum: usize,
    record: RunRules,
}
impl MessageLimitPlugin {
    /// Count system, task, replies, tool results, and standalone feedback.
    pub fn new(maximum: usize) -> Self {
        Self {
            maximum,
            record: RunRules::inactive(),
        }
    }
    /// Also report checked counts in this experiment's record.
    pub fn with_record(mut self, record: RunRules) -> Self {
        self.record = record;
        self
    }
}
impl Plugin for MessageLimitPlugin {
    const ID: &'static str = "polaris_guards::message_limit";
    const VERSION: Version = Version::new(0, 1, 0);
    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<CodeAgentPlugin>()]
    }
    fn build(&self, server: &mut Server) {
        let maximum = self.maximum;
        let rules = self.record.clone();
        server
            .get_resource_mut::<CodeAgentSteps>()
            .unwrap()
            .configure_session("conversation.limit", move |ctx| {
                let mut context = ctx
                    .get_resource_mut::<ContextManager>()
                    .map_err(polaris::agent::SetupError::new)?;
                context.set_message_limit(maximum);
                let rules = rules.clone();
                context.observe_message_count(move |count, failure| {
                    rules.note_messages(count, failure)
                });
                Ok(())
            })
            .unwrap();
    }
}

/// Records model attempts and completed turns; use it to retain experiment evidence and project the checked final report.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`TurnObserver`] | Global | Finalizes the record and returns the report projection. |
///
/// # APIs Provided
///
/// None; this plugin composes stock resources and registries.
///
/// # Dependencies
///
/// [`CodeAgentPlugin`]. Configures the existing `AgentConfig` attempt observer.
///
/// # Lifecycle
///
/// Contributions are registered during `build` and frozen with stock registries.
/// No worker tasks are spawned. Session setup runs on creation and resume.
///
/// # Example
///
/// ```no_run
/// use polaris::system::server::Server;
/// use polaris_code::agents::CodeAgentPlugin;
/// use polaris_guards::rules::{Arm, RulesConfig, RunRules};
/// use polaris_guards::rules::plugins::*;
/// let rules = RunRules::new(RulesConfig::new(Arm::Prompt), std::path::Path::new("."))?;
/// let mut server = Server::new();
/// // Add the stock models, tools and sessions before the coding agent.
/// server.add_plugins(CodeAgentPlugin::new());
/// server.add_plugins(RunRecordPlugin(rules));
/// # Ok::<(), String>(())
/// ```
pub struct RunRecordPlugin(pub RunRules);
impl Plugin for RunRecordPlugin {
    const ID: &'static str = "polaris_guards::record";
    const VERSION: Version = Version::new(0, 1, 0);
    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<CodeAgentPlugin>()]
    }
    fn build(&self, server: &mut Server) {
        let rules = self.0.clone();
        server
            .get_resource_mut::<CodeAgentSteps>()
            .unwrap()
            .configure_session("rules.record", move |ctx| {
                let rules = rules.clone();
                ctx.get_resource_mut::<AgentConfig>()
                    .map_err(polaris::agent::SetupError::new)?
                    .model_observer = Some(ModelObserver::new(move |model, attempt| {
                    rules.set_model(model);
                    rules.model_call(ModelCall {
                        index: 0,
                        at_ms: 0,
                        ok: attempt.ok,
                        input_tokens: attempt.input_tokens,
                        output_tokens: attempt.output_tokens,
                        cache_read_tokens: attempt.cache_read_tokens,
                        cache_creation_tokens: attempt.cache_creation_tokens,
                        stop_reason: attempt.stop_reason,
                    });
                }));
                Ok(())
            })
            .unwrap();
        let rules = self.0.clone();
        server.insert_global(TurnObserver::new(move |history, mut outcome| {
            let rules = rules.clone();
            async move {
                rules
                    .finish(EndInput {
                        reason: &outcome.reason,
                        history: &history,
                        model_final_text: outcome.final_text.as_deref(),
                        usage: outcome
                            .usage
                            .as_ref()
                            .map(|usage| serde_json::to_value(usage).unwrap()),
                        exit_code: outcome.reason.exit_code(),
                    })
                    .await;
                if let Some(report) = rules.reported_final() {
                    outcome.final_text = Some(report);
                }
                outcome
            }
        }));
    }
}
