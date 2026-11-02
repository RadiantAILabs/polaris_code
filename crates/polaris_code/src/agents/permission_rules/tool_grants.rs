use std::fmt;

use polaris::system::resource::GlobalResource;
use polaris::tools::ToolPermission;

use crate::agents::PermissionPolicy;
use crate::tools::FILE_WRITE_TOOLS;
use crate::tools::mcp::MCP_TOOL_PREFIX;

/// Why a launch-time tool grant was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolGrantError {
    /// The pattern is empty, has a wildcard other than one trailing `*`, or
    /// contains whitespace.
    InvalidPattern(String),
    /// A wildcard named an MCP tool; those need an exact name.
    McpWildcard(String),
}

impl fmt::Display for ToolGrantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPattern(pattern) => write!(
                f,
                "invalid --allow-tool pattern {pattern:?}; use an exact name or a trailing *"
            ),
            Self::McpWildcard(_) => write!(
                f,
                "--allow-tool requires an exact MCP tool name; MCP wildcard grants are refused"
            ),
        }
    }
}

impl std::error::Error for ToolGrantError {}

impl From<ToolGrantError> for String {
    fn from(error: ToolGrantError) -> Self {
        error.to_string()
    }
}

/// Explicit operator grants for additional tools in one unattended run.
///
/// Names or trailing `*` prefixes are accepted. MCP tools require exact names;
/// a generic wildcard never grants an MCP tool. File writers and shell tools
/// retain their dedicated `--allow-edit`/`--allow-command` gates. Grants apply
/// only to an unattended policy and never override a deny rule.
///
/// # Scope
///
/// Global: the grants come from one launch command line and apply to every call
/// of the process.
///
/// # Provided By
///
/// [`PermissionRulesPlugin`](super::PermissionRulesPlugin) publishes the grants
/// it was given with `with_tool_grants`; the default grants nothing.
///
/// # Access
///
/// Read with `Res<RunToolGrants>`. The grants are validated once and immutable
/// afterwards, with no interior mutability.
///
/// # Alternatives
///
/// File writers and shell commands use their own gates
/// ([`UnattendedGrants`](crate::agents::UnattendedGrants)); `allow` rules in a
/// permission file widen only attended calls.
///
/// # Example
///
/// ```
/// use polaris::system::param::{Res, ResMut};
/// use polaris::system::system;
/// use polaris_code::agents::PolicySnapshot;
/// use polaris_code::agents::permission_rules::RunToolGrants;
///
/// #[system]
/// async fn apply_launch_grants(grants: Res<RunToolGrants>, mut policy: ResMut<PolicySnapshot>) {
///     grants.apply(&mut policy.permission, ["web_fetch", "mcp__repo__search"]);
/// }
/// ```
#[derive(Debug, Clone, Default)]
pub struct RunToolGrants(Vec<String>);

impl GlobalResource for RunToolGrants {}

impl RunToolGrants {
    /// Validate immutable launch-time grants.
    ///
    /// # Errors
    /// Rejects empty names, non-prefix wildcards, and wildcard MCP grants.
    pub fn new(patterns: Vec<String>) -> Result<Self, ToolGrantError> {
        for pattern in &patterns {
            let name = pattern.strip_suffix('*').unwrap_or(pattern);
            if pattern.is_empty()
                || name.contains(['*', '?', '[', ']'])
                || name.chars().any(char::is_whitespace)
            {
                return Err(ToolGrantError::InvalidPattern(pattern.clone()));
            }
            if pattern.starts_with(MCP_TOOL_PREFIX) && pattern.contains('*') {
                return Err(ToolGrantError::McpWildcard(pattern.clone()));
            }
        }
        Ok(Self(patterns))
    }

    /// Apply only the launch-time grants to an unattended policy for the
    /// concrete names in a batch. Rule files may subsequently narrow them.
    pub fn apply<'a>(
        &self,
        policy: &mut PermissionPolicy,
        names: impl IntoIterator<Item = &'a str>,
    ) {
        if !policy.is_unattended() {
            return;
        }
        for name in names {
            if policy.overrides.get(name) == Some(&ToolPermission::Deny)
                || FILE_WRITE_TOOLS.contains(&name)
                || super::super::policy::SHELL_EXECUTION_TOOLS.contains(&name)
            {
                continue;
            }
            if self.0.iter().any(|pattern| {
                pattern == name
                    || (!name.starts_with(MCP_TOOL_PREFIX)
                        && pattern
                            .strip_suffix('*')
                            .is_some_and(|prefix| name.starts_with(prefix)))
            }) {
                policy.overrides.insert(name.into(), ToolPermission::Allow);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{PermissionMode, UnattendedGrants};

    #[test]
    fn unattended_tool_patterns_require_exact_mcp_names_and_preserve_dedicated_gates() {
        let grants = RunToolGrants::new(vec!["*".into(), "mcp__repo__search".into()]).unwrap();
        let mut policy =
            PermissionMode::Unattended(UnattendedGrants::default()).permission_policy();
        grants.apply(
            &mut policy,
            [
                "web_fetch",
                "mcp__repo__search",
                "mcp__other__write",
                "edit_file",
                "run_command",
            ],
        );
        assert_eq!(
            policy.resolve("web_fetch", ToolPermission::Confirm),
            ToolPermission::Allow
        );
        assert_eq!(
            policy.overrides.get("mcp__repo__search"),
            Some(&ToolPermission::Allow)
        );
        assert!(!policy.overrides.contains_key("mcp__other__write"));
        assert_eq!(
            policy.resolve("edit_file", ToolPermission::Confirm),
            ToolPermission::Deny
        );
        assert_eq!(
            policy.resolve("run_command", ToolPermission::Allow),
            ToolPermission::Deny
        );
        for pattern in ["mcp__repo__*", "foo*bar", "foo?", ""] {
            assert!(RunToolGrants::new(vec![pattern.into()]).is_err());
        }
        let mut attended = PermissionMode::Confirm.permission_policy();
        grants.apply(&mut attended, ["future_mutation"]);
        assert_eq!(
            attended.resolve("future_mutation", ToolPermission::Confirm),
            ToolPermission::Confirm
        );
    }
}
