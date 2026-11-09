//! The permission posture a run starts in, and the shell and policy settings
//! that follow from it.

use std::path::Path;

use polaris::shell::ShellConfig;
use polaris::tools::ToolPermission;

use super::cli::Cli;
use crate::agents::policy::RetryPolicy;
use crate::agents::{
    AgentConfig, PermissionCeiling, PermissionMode, PolicySnapshot, UnattendedGrants,
};
use crate::tools::GraphSearchMode;

/// Destructive command patterns refused outright by the shell, regardless of
/// the per-tool confirm gate — a backstop, not the primary permission layer.
const DENIED_COMMANDS: &[&str] = &["rm -rf *", "rm -rf /", "sudo *", ":(){ :|:& };:", "mkfs *"];

/// Resolve the posture this process starts in, and the ceiling it may never
/// exceed, from the operator's flags.
///
/// Three rules, in the order they apply:
///
/// * **`--print` takes neither flag.** It already refuses `--permission-mode`;
///   it refuses `--permission-ceiling` for that reason and one more. An
///   unattended posture is incomparable with the attended ladder a ceiling is
///   defined over (`PermissionMode::attended_rung`), so a ceiling over a
///   `--print` run could only do nothing or silently redefine the run's grants
///   — and the second is precisely "the ceiling became a way to express a
///   `--print` permission mode". The posture of a `--print` run is already
///   immutable and already fixed by the same operator on the same command
///   line, through `--allow-edit` / `--allow-command`.
/// * **An explicit `--permission-mode` above the ceiling is refused**, not
///   clamped: the operator wrote both flags, and a contradiction between them
///   is worth a line of output rather than a silent reinterpretation.
/// * **The implicit default is clamped into the ceiling.** `confirm` is the
///   default posture, so `--permission-ceiling plan` on its own starts the
///   session in `plan`. Clamping only ever lowers.
pub(super) fn startup_permissions(
    cli: &Cli,
    headless: bool,
) -> Result<(PermissionMode, Option<PermissionCeiling>), String> {
    if headless {
        crate::agents::permission_rules::RunToolGrants::new(cli.allow_tool.clone())?;
        if cli.permission_mode.is_some() {
            return Err(
                "--permission-mode is interactive-only; a non-interactive run uses unattended \
                 grants"
                    .into(),
            );
        }
        if cli.permission_ceiling.is_some() {
            return Err(
                "--permission-ceiling is interactive-only; a --print run is already \
                 unattended and its posture is fixed by --allow-edit/--allow-command"
                    .into(),
            );
        }
        return Ok((
            PermissionMode::Unattended(UnattendedGrants {
                edit_file: cli.allow_edit || cli.dangerously_skip_permissions,
                run_command_patterns: if cli.dangerously_skip_permissions {
                    vec!["*".into()]
                } else {
                    cli.allow_command.clone()
                },
            }),
            None,
        ));
    }

    let ceiling = match cli.permission_ceiling.map(PermissionMode::from) {
        // `InteractivePermissionMode` has no unattended variant, so this is
        // unreachable today; it stays a refusal rather than an `expect` so the
        // one posture that cannot be a ceiling can never become one by a later
        // edit to that enum.
        Some(mode) => Some(
            PermissionCeiling::new(mode.clone())
                .ok_or_else(|| format!("`{}` cannot be a permission ceiling", mode.label()))?,
        ),
        None => None,
    };
    let mode = match (&ceiling, cli.permission_mode.map(PermissionMode::from)) {
        (Some(ceiling), Some(requested)) if !ceiling.admits(&requested) => {
            return Err(format!(
                "{}; choose a mode at or below the ceiling",
                ceiling.refusal(requested.label())
            ));
        }
        (_, Some(requested)) => requested,
        (Some(ceiling), None) => ceiling.clamp(PermissionMode::default()),
        (None, None) => PermissionMode::default(),
    };
    Ok((mode, ceiling))
}

/// The one line an interactive session opens with when a ceiling is in force.
///
/// A posture the user cannot raise has to be readable without asking for it, so
/// the session says so up front; `/permission` then advertises only the modes
/// the ceiling admits and names the ceiling in every refusal.
pub(super) fn ceiling_notice(ceiling: &PermissionCeiling, active: &PermissionMode) -> String {
    format!(
        "Permission ceiling `{}` is in force for this process. This session starts in `{}`, \
         and nothing inside it — a command, a setting or a prompt — can raise the posture \
         above the ceiling.",
        ceiling.label(),
        active.label()
    )
}

/// Default seconds a shell command may run when a person is watching: they can
/// send a long run to the background or stop it themselves.
const ATTENDED_COMMAND_TIMEOUT_SECS: u64 = 120;

/// Default seconds a shell command may run in an unattended run. Nobody can
/// send a long build or test run to the background there, so the default
/// leaves room for one. A call's own `timeout_secs` still overrides it.
const UNATTENDED_COMMAND_TIMEOUT_SECS: u64 = 600;

/// The default shell command timeout for a run in `mode`.
fn command_timeout_secs(mode: &PermissionMode) -> u64 {
    match mode {
        PermissionMode::Unattended(_) => UNATTENDED_COMMAND_TIMEOUT_SECS,
        PermissionMode::Confirm
        | PermissionMode::AutoEdit
        | PermissionMode::Auto
        | PermissionMode::Plan => ATTENDED_COMMAND_TIMEOUT_SECS,
    }
}

pub(super) fn configured_shell(
    working_dir: &Path,
    mode: &PermissionMode,
    unrestricted: bool,
) -> ShellConfig {
    let allowed_commands = match mode {
        PermissionMode::Unattended(grants) => grants.run_command_patterns.clone(),
        PermissionMode::Confirm
        | PermissionMode::AutoEdit
        | PermissionMode::Auto
        | PermissionMode::Plan => {
            vec!["*".to_string()]
        }
    };
    ShellConfig::new()
        .with_working_dir(working_dir)
        .with_timeout(command_timeout_secs(mode))
        .with_max_output_bytes(64 * 1024)
        .with_allowed_dirs(vec![working_dir.to_path_buf()])
        .with_allowed_commands(allowed_commands)
        .with_denied_commands(if unrestricted {
            Vec::new()
        } else {
            DENIED_COMMANDS.iter().map(|s| (*s).to_string()).collect()
        })
}

/// Keep the host's activated project runtime (conda env, virtualenv) in
/// shell children when `POLARIS_CODE_TESTENV=1`. Only runtime-setup names
/// pass; full inheritance would also hand provider credentials to
/// model-authored scripts. The benchmark harness activates the image's
/// environment before it starts this process.
pub(super) fn project_shell_environment(config: ShellConfig, enabled: bool) -> ShellConfig {
    if !enabled {
        return config;
    }
    config.with_env_allowlist(
        [
            "PATH",
            "CONDA_PREFIX",
            "CONDA_DEFAULT_ENV",
            "CONDA_SHLVL",
            "VIRTUAL_ENV",
            "PYTHONPATH",
            "LD_LIBRARY_PATH",
            "LANG",
            "LC_ALL",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    )
}

/// The policy a session of this run starts with.
///
/// Sized to the agent's declared model limits, with `mode`'s permissions. An
/// unattended run gets `unattended_nudges`; an attended one never needs them,
/// because a person answers a reply that calls no tool.
pub(super) fn policy_snapshot(
    config: &AgentConfig,
    mode: &PermissionMode,
    graph_search: GraphSearchMode,
    unattended_nudges: u32,
) -> PolicySnapshot {
    let mut policy = PolicySnapshot::for_agent(config);
    policy.permission = mode.permission_policy();
    if matches!(mode, PermissionMode::Unattended(_)) {
        policy.continuation = policy
            .continuation
            .with_unattended_nudges(unattended_nudges);
        policy.retry = RetryPolicy::unattended();
    }
    if !graph_search.freeform_enabled() {
        policy.tool_palette.denied.push("sparql_query".to_string());
        policy
            .permission
            .overrides
            .insert("sparql_query".to_string(), ToolPermission::Deny);
    }
    policy
}

#[cfg(test)]
mod tests {
    use super::super::cli::{Cli, parse as cli, run_max_turns};
    use super::*;
    use crate::agents::DEFAULT_MAX_TURNS;
    use clap::Parser;
    use polaris::shell::{ShellExecutor, ShellPermission};
    use std::num::NonZeroU32;

    #[test]
    fn the_session_policy_keeps_model_limits_and_nudges_only_unattended_runs() {
        let root = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::new("m", root.path())
            .with_max_output_tokens(NonZeroU32::new(131_072).unwrap());
        let unsized_policy = policy_snapshot(
            &config,
            &PermissionMode::Confirm,
            GraphSearchMode::default(),
            1,
        );
        assert_eq!(
            unsized_policy.retrieval.max_tokens,
            Some(crate::agents::policy::DEFAULT_MAX_WINDOW_TOKENS)
        );
        assert_eq!(unsized_policy.continuation.unattended_nudges, 0);
        assert_eq!(unsized_policy.retry, RetryPolicy::default());

        config.context_window = Some(NonZeroU32::new(1_048_576).unwrap());
        let unattended = policy_snapshot(
            &config,
            &PermissionMode::Unattended(UnattendedGrants::default()),
            GraphSearchMode::default(),
            2,
        );
        let input = 1_048_576 - 131_072;
        assert_eq!(unattended.retrieval.max_tokens, Some(input * 4 / 5));
        assert_eq!(unattended.continuation.unattended_nudges, 2);
        assert_eq!(unattended.retry, RetryPolicy::unattended());
        assert!(unattended.permission.is_unattended());
    }

    #[test]
    fn unattended_runs_give_shell_commands_a_longer_default_timeout() {
        let root = tempfile::tempdir().unwrap();
        let unattended = PermissionMode::Unattended(UnattendedGrants::default());
        assert_eq!(
            configured_shell(root.path(), &unattended, false).default_timeout_secs,
            UNATTENDED_COMMAND_TIMEOUT_SECS
        );
        for attended in [
            PermissionMode::Confirm,
            PermissionMode::AutoEdit,
            PermissionMode::Auto,
            PermissionMode::Plan,
        ] {
            assert_eq!(
                configured_shell(root.path(), &attended, false).default_timeout_secs,
                ATTENDED_COMMAND_TIMEOUT_SECS,
                "{}",
                attended.label()
            );
        }
    }

    fn resolve(args: &[&str]) -> Result<(PermissionMode, Option<PermissionCeiling>), String> {
        let parsed = cli(args);
        let headless = parsed.print.is_some();
        startup_permissions(&parsed, headless)
    }

    #[test]
    fn a_startup_mode_above_the_ceiling_is_refused_and_names_it() {
        let refusal = resolve(&[
            "--permission-ceiling",
            "confirm",
            "--permission-mode",
            "auto-edit",
        ])
        .expect_err("`auto-edit` exceeds a `confirm` ceiling");
        assert!(refusal.contains("ceiling"), "{refusal}");
        assert!(refusal.contains("confirm"), "{refusal}");
        assert!(refusal.contains("auto-edit"), "{refusal}");
    }

    #[test]
    fn a_startup_mode_at_or_below_the_ceiling_is_kept() {
        for requested in ["plan", "confirm"] {
            let (mode, ceiling) = resolve(&[
                "--permission-ceiling",
                "confirm",
                "--permission-mode",
                requested,
            ])
            .expect("a mode at or below the ceiling starts");
            assert_eq!(mode.label(), requested);
            assert_eq!(ceiling.expect("the ceiling is in force").label(), "confirm");
        }
    }

    /// The default posture is `confirm`, which a stricter ceiling lowers rather
    /// than being bypassed by.
    #[test]
    fn the_default_posture_is_clamped_into_the_ceiling() {
        let (mode, ceiling) =
            resolve(&["--permission-ceiling", "plan"]).expect("a ceiling alone starts a session");
        assert_eq!(mode, PermissionMode::Plan);
        assert_eq!(ceiling.expect("the ceiling is in force").label(), "plan");

        let (mode, _) = resolve(&["--permission-ceiling", "auto-edit"])
            .expect("a ceiling above the default starts a session");
        assert_eq!(
            mode,
            PermissionMode::Confirm,
            "a ceiling above the default must not raise it"
        );
    }

    #[test]
    fn an_unconstrained_start_is_unchanged() {
        let (mode, ceiling) = resolve(&[]).expect("the plain interactive start");
        assert_eq!(mode, PermissionMode::Confirm);
        assert!(ceiling.is_none());

        let (mode, ceiling) =
            resolve(&["--permission-mode", "auto-edit"]).expect("an explicit interactive posture");
        assert_eq!(mode, PermissionMode::AutoEdit);
        assert!(ceiling.is_none());
    }

    #[test]
    fn auto_permission_mode_is_attended_and_requires_explicit_ceiling_admission() {
        let (mode, ceiling) = resolve(&["--permission-mode", "auto"]).unwrap();
        assert_eq!(mode, PermissionMode::Auto);
        assert!(ceiling.is_none());
        for ceiling in ["plan", "confirm", "auto-edit"] {
            assert!(
                resolve(&["--permission-mode", "auto", "--permission-ceiling", ceiling]).is_err()
            );
        }
        assert!(resolve(&["--permission-mode", "auto", "--permission-ceiling", "auto"]).is_ok());
        assert!(resolve(&["--print", "hello", "--permission-mode", "auto"]).is_err());
        let parsed = cli(&["--review-model", "scripted/reviewer"]);
        assert_eq!(parsed.review_model.as_deref(), Some("scripted/reviewer"));
    }

    #[test]
    fn unattended_additional_tool_grants_validate_at_startup() {
        assert!(Cli::try_parse_from(["polaris", "--allow-tool", "web_fetch"]).is_err());
        let parsed = cli(&[
            "--print",
            "hello",
            "--allow-tool",
            "web_fetch",
            "--allow-tool",
            "mcp__repo__search",
        ]);
        assert!(startup_permissions(&parsed, true).is_ok());
        let parsed = cli(&["--print", "hello", "--allow-tool", "mcp__repo__*"]);
        assert!(
            startup_permissions(&parsed, true)
                .unwrap_err()
                .contains("exact MCP")
        );
    }

    /// `--print` stays fail-closed unattended and takes neither flag: its
    /// posture is fixed by `--allow-edit`/`--allow-command` on the same command
    /// line, and a ceiling must not become a second way to express one.
    #[test]
    fn print_remains_fail_closed_and_takes_neither_permission_flag() {
        assert!(
            resolve(&["--print", "hi", "--permission-mode", "plan"])
                .expect_err("--permission-mode is interactive-only")
                .contains("--permission-mode")
        );
        let refusal = resolve(&["--print", "hi", "--permission-ceiling", "plan"])
            .expect_err("--permission-ceiling is interactive-only");
        assert!(refusal.contains("--permission-ceiling"), "{refusal}");

        let (mode, ceiling) = resolve(&["--print", "hi"]).expect("a plain unattended run");
        assert_eq!(
            mode,
            PermissionMode::Unattended(UnattendedGrants::default()),
            "an ungranted --print run stays fail-closed"
        );
        assert!(ceiling.is_none(), "a --print run carries no ceiling");

        let (mode, _) = resolve(&[
            "--print",
            "hi",
            "--allow-edit",
            "--allow-command",
            "cargo *",
        ])
        .expect("a granted unattended run");
        assert_eq!(
            mode,
            PermissionMode::Unattended(UnattendedGrants {
                edit_file: true,
                run_command_patterns: vec!["cargo *".into()],
            })
        );
    }

    #[test]
    fn unrestricted_permissions_require_an_explicit_print_flag() {
        assert_eq!(
            Cli::try_parse_from(["polaris", "--dangerously-skip-permissions"])
                .err()
                .expect("print is required")
                .kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        let parsed = cli(&["-p", "hello", "--dangerously-skip-permissions"]);
        let (mode, ceiling) = startup_permissions(&parsed, true).unwrap();
        assert_eq!(
            mode,
            PermissionMode::Unattended(UnattendedGrants {
                edit_file: true,
                run_command_patterns: vec!["*".into()],
            })
        );
        assert!(ceiling.is_none());
        assert_eq!(run_max_turns(&parsed), usize::MAX);
        assert_eq!(run_max_turns(&cli(&["-p", "hello"])), DEFAULT_MAX_TURNS);
        assert_eq!(
            run_max_turns(&cli(&[
                "-p",
                "hello",
                "--dangerously-skip-permissions",
                "--max-turns",
                "7"
            ])),
            7
        );
        let root = tempfile::tempdir().unwrap();
        let config = configured_shell(root.path(), &mode, true);
        assert!(config.denied_commands.is_empty());
        assert!(config.sandbox);
        assert_eq!(config.allowed_dirs, vec![root.path().to_path_buf()]);
        assert!(
            !configured_shell(root.path(), &mode, false)
                .denied_commands
                .is_empty()
        );
    }

    /// The session opens by saying what the user cannot do, in both labels.
    #[test]
    fn the_opening_notice_names_the_ceiling_and_the_starting_posture() {
        let notice = ceiling_notice(
            &PermissionCeiling::new(PermissionMode::Confirm).expect("confirm is a ceiling"),
            &PermissionMode::Plan,
        );
        assert!(notice.contains("ceiling"), "{notice}");
        assert!(notice.contains("`confirm`"), "{notice}");
        assert!(notice.contains("`plan`"), "{notice}");
    }

    #[test]
    fn attended_shell_policy_keeps_the_deny_list_and_directory_sandbox() {
        let working_dir = std::env::current_dir().expect("current dir");
        for mode in [
            PermissionMode::Confirm,
            PermissionMode::AutoEdit,
            PermissionMode::Plan,
        ] {
            let executor = ShellExecutor::new(configured_shell(&working_dir, &mode, false));
            assert_eq!(executor.check_permission("echo ok"), ShellPermission::Allow);
            assert_eq!(
                executor.check_permission("rm -rf build"),
                ShellPermission::Deny
            );
            assert!(executor.config().sandbox);
            assert_eq!(executor.config().allowed_dirs, vec![working_dir.clone()]);
        }
    }

    #[test]
    fn unattended_shell_policy_allows_only_declared_patterns() {
        let working_dir = std::env::current_dir().expect("current dir");
        let mode = PermissionMode::Unattended(UnattendedGrants {
            edit_file: false,
            run_command_patterns: vec!["cargo *".into()],
        });
        let executor = ShellExecutor::new(configured_shell(&working_dir, &mode, false));
        assert_eq!(
            executor.check_permission("cargo test --workspace"),
            ShellPermission::Allow
        );
        assert_eq!(
            executor.check_permission("echo ok"),
            ShellPermission::Confirm
        );
        assert_eq!(
            executor.check_permission("rm -rf build"),
            ShellPermission::Deny
        );
        assert_eq!(executor.config().allowed_dirs, vec![working_dir]);
    }

    #[test]
    fn project_environment_keeps_runtime_names_only_when_enabled() {
        let dir = std::env::current_dir().unwrap();
        let mode = PermissionMode::Unattended(UnattendedGrants {
            edit_file: false,
            run_command_patterns: vec!["python *".into()],
        });
        let off = project_shell_environment(configured_shell(&dir, &mode, false), false);
        assert!(off.env_allowlist.is_empty());
        let on = project_shell_environment(configured_shell(&dir, &mode, false), true);
        assert!(!on.inherit_env);
        assert!(on.env_allowlist.contains(&"PATH".to_string()));
        assert!(on.env_allowlist.contains(&"CONDA_PREFIX".to_string()));
        for secret in [
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "ANTHROPIC_API_KEY",
        ] {
            assert!(!on.env_allowlist.contains(&secret.to_string()));
        }
    }
}
