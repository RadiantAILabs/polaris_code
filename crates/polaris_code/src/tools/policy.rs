//! Policy facts a tool capability publishes about its own tools.
//!
//! A capability knows things about its tools that agent policy cannot learn
//! from the registry's [`ToolPermission`](polaris::tools::ToolPermission) alone:
//! whether an operator vouched that
//! a tool only reads, and whether every attended call must be answered by a
//! person. [`ToolPolicyPlugin`] provides one [`ToolPolicyRegistry`] that the
//! capabilities declare these facts into while they build. Agent policy reads
//! them back and decides what each mode does with them; the capability never
//! names a mode, and the agent never names a capability.

use std::collections::HashMap;
use std::sync::Arc;

use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version};
use polaris::system::resource::GlobalResource;
use polaris::system::server::Server;

/// What a capability asserts a tool does to state outside this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolEffect {
    /// The tool only reads. Modes that refuse mutation, such as `plan`, run it
    /// without asking.
    ReadOnly,
    /// The tool may change state outside this process. Modes that refuse
    /// mutation refuse it.
    Mutating,
}

/// Who may answer an attended call to a tool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum AttendedApproval {
    /// Rules, grants and the permission mode decide, and the automatic reviewer
    /// may settle a call that is left to ask.
    #[default]
    ByPolicy,
    /// Every attended call asks a person. No allow rule, session grant or tool
    /// override removes the prompt, and the automatic reviewer never sees the
    /// call. [`ToolEffect::ReadOnly`] in `plan` is the one allowance.
    Human,
}

/// The facts one capability declares about one tool.
///
/// The default declares nothing: the tool follows its registry permission and
/// the mode's own overrides, as any tool does.
///
/// # Examples
///
/// ```
/// use polaris_code::tools::{AttendedApproval, ToolEffect, ToolPolicy};
///
/// let policy = ToolPolicy::default()
///     .with_effect(ToolEffect::ReadOnly)
///     .with_attended(AttendedApproval::Human);
/// assert_eq!(policy.effect(), Some(ToolEffect::ReadOnly));
/// assert_eq!(policy.attended(), AttendedApproval::Human);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ToolPolicy {
    effect: Option<ToolEffect>,
    attended: AttendedApproval,
}

impl ToolPolicy {
    /// State what the tool does to state outside this process.
    #[must_use]
    pub fn with_effect(mut self, effect: ToolEffect) -> Self {
        self.effect = Some(effect);
        self
    }

    /// State who may answer an attended call.
    #[must_use]
    pub fn with_attended(mut self, attended: AttendedApproval) -> Self {
        self.attended = attended;
        self
    }

    /// The declared effect, or `None` when the capability made no claim.
    #[must_use]
    pub fn effect(&self) -> Option<ToolEffect> {
        self.effect
    }

    /// Who may answer an attended call.
    #[must_use]
    pub fn attended(&self) -> AttendedApproval {
        self.attended
    }
}

/// Policy facts declared by tool capabilities, keyed by tool name.
///
/// # Scope
///
/// Global: the facts describe tools of the process and apply to every session.
///
/// # Provided By
///
/// [`ToolPolicyPlugin`] inserts it as a mutable resource during `build` so
/// capabilities can declare into it, and freezes it into a global during
/// `ready`.
///
/// # Access
///
/// Capabilities call [`declare`](Self::declare) through
/// `Server::get_resource_mut` while they build. Systems read it with
/// `Res<ToolPolicyRegistry>`; the frozen registry has no mutation path, and
/// cloning it shares the declarations.
///
/// # Alternatives
///
/// [`ToolRegistry`](polaris::tools::ToolRegistry) holds the permission a tool
/// registers with; this registry holds facts that permission cannot express.
///
/// # Example
///
/// ```
/// use polaris::system::param::Res;
/// use polaris::system::system;
/// use polaris_code::tools::{AttendedApproval, ToolPolicyRegistry};
///
/// #[system]
/// async fn needs_a_person(tools: Res<ToolPolicyRegistry>) -> bool {
///     tools.get("web_fetch").attended() == AttendedApproval::Human
/// }
/// ```
#[derive(Debug, Clone, Default)]
pub struct ToolPolicyRegistry {
    policies: Arc<HashMap<String, ToolPolicy>>,
}

impl GlobalResource for ToolPolicyRegistry {}

impl Contract for ToolPolicyRegistry {
    const CONTRACT_VERSION: Version = Version::new(0, 1, 0);
}

impl ToolPolicyRegistry {
    /// Declare the facts for `tool`, replacing an earlier declaration.
    pub fn declare(&mut self, tool: impl Into<String>, policy: ToolPolicy) {
        Arc::make_mut(&mut self.policies).insert(tool.into(), policy);
    }

    /// The facts declared for `tool`; the default declares nothing.
    #[must_use]
    pub fn get(&self, tool: &str) -> ToolPolicy {
        self.policies.get(tool).copied().unwrap_or_default()
    }
}

/// Provides the [`ToolPolicyRegistry`] that tool capabilities declare into.
///
/// Add this plugin when an assembly has capabilities that publish policy facts
/// about their tools, or an agent that reads them. The capabilities in this
/// crate that publish facts ([`WebToolsPlugin`](super::WebToolsPlugin) and
/// [`McpClientPlugin`](super::mcp::McpClientPlugin)) and the agent policy that
/// reads them install it as a default dependency, so most assemblies never name
/// it.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`ToolPolicyRegistry`] | Global | Facts capabilities declare about their tools: read-only effect and whether every attended call needs a person |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | _none_ | The registry is a resource, not an API; capabilities declare into it during `build`. |
///
/// # Dependencies
///
/// None. Capabilities that declare facts depend on this plugin.
///
/// # Lifecycle
///
/// - **`build()`** — inserts an empty registry as a mutable resource.
/// - **`ready()`** — moves it to a global for read-only access.
///
/// # Example
///
/// ```
/// use polaris::system::server::Server;
/// use polaris_code::tools::{AttendedApproval, ToolPolicy, ToolPolicyPlugin, ToolPolicyRegistry};
///
/// # async fn example() {
/// let mut server = Server::new();
/// server.add_plugins(ToolPolicyPlugin);
/// server.finish().await.unwrap();
/// let registry = server.get_global::<ToolPolicyRegistry>().unwrap();
/// assert_eq!(registry.get("anything").attended(), AttendedApproval::ByPolicy);
/// # }
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct ToolPolicyPlugin;

impl Plugin for ToolPolicyPlugin {
    const ID: &'static str = "polaris_code::tool_policy";
    const VERSION: Version = Version::new(0, 1, 0);

    fn access(&self) -> PluginAccess {
        PluginAccess::new().provides::<ToolPolicyRegistry>(ToolPolicyRegistry::CONTRACT_VERSION)
    }

    fn build(&self, server: &mut Server) {
        server.insert_resource(ToolPolicyRegistry::default());
    }

    async fn ready(&self, server: &mut Server) {
        let Some(registry) = server.remove_resource::<ToolPolicyRegistry>() else {
            tracing::warn!("tool policy registry missing at ready; declarations were lost");
            return;
        };
        server.insert_global(registry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris::system::plugin::{PluginId, VersionReq};

    /// A capability that declares one tool, written the way the real ones are.
    struct Declares(&'static str, ToolPolicy);

    impl Plugin for Declares {
        const ID: &'static str = "test::declares";
        const VERSION: Version = Version::new(0, 1, 0);

        fn dependencies(&self) -> Vec<PluginId> {
            vec![PluginId::of::<ToolPolicyPlugin>()]
        }

        fn access(&self) -> PluginAccess {
            PluginAccess::new().extends::<ToolPolicyRegistry>(VersionReq::caret(
                ToolPolicyRegistry::CONTRACT_VERSION,
            ))
        }

        fn build(&self, server: &mut Server) {
            server
                .get_resource_mut::<ToolPolicyRegistry>()
                .expect("ToolPolicyPlugin must be added before a declaring plugin")
                .declare(self.0, self.1);
        }
    }

    #[test]
    fn an_undeclared_tool_makes_no_claim() {
        let registry = ToolPolicyRegistry::default();
        assert_eq!(registry.get("read_file"), ToolPolicy::default());
        assert_eq!(registry.get("read_file").effect(), None);
        assert_eq!(
            registry.get("read_file").attended(),
            AttendedApproval::ByPolicy
        );
    }

    #[test]
    fn a_later_declaration_replaces_an_earlier_one_and_clones_share_the_result() {
        let mut registry = ToolPolicyRegistry::default();
        registry.declare("t", ToolPolicy::default().with_effect(ToolEffect::ReadOnly));
        registry.declare("t", ToolPolicy::default().with_effect(ToolEffect::Mutating));
        let copy = registry.clone();
        assert_eq!(copy.get("t").effect(), Some(ToolEffect::Mutating));
    }

    #[tokio::test]
    async fn declarations_reach_the_frozen_global_whatever_the_registration_order() {
        let policy = ToolPolicy::default()
            .with_effect(ToolEffect::ReadOnly)
            .with_attended(AttendedApproval::Human);
        let mut forward = Server::new();
        forward
            .add_plugins(ToolPolicyPlugin)
            .add_plugins(Declares("probe", policy));
        forward.finish().await.unwrap();
        let mut reverse = Server::new();
        reverse
            .add_plugins(Declares("probe", policy))
            .add_plugins(ToolPolicyPlugin);
        reverse.finish().await.unwrap();
        for server in [&forward, &reverse] {
            let registry = server.get_global::<ToolPolicyRegistry>().unwrap();
            assert_eq!(registry.get("probe"), policy);
            assert_eq!(registry.get("other"), ToolPolicy::default());
        }
    }
}
