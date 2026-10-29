//! Mode-by-mode permission resolution at the public policy boundary.

use polaris::tools::ToolPermission;
use polaris_code::agents::{PermissionMode, UnattendedGrants};

fn permission(mode: &PermissionMode, tool: &str, default: ToolPermission) -> ToolPermission {
    mode.permission_policy().resolve(tool, default)
}

#[test]
fn confirm_prompts_for_both_mutating_tools() {
    let mode = PermissionMode::Confirm;
    assert_eq!(
        permission(&mode, "edit_file", ToolPermission::Confirm),
        ToolPermission::Confirm
    );
    assert_eq!(
        permission(&mode, "run_command", ToolPermission::Allow),
        ToolPermission::Confirm
    );
    assert_eq!(
        permission(&mode, "read_file", ToolPermission::Allow),
        ToolPermission::Allow
    );
}

#[test]
fn auto_edit_runs_edits_but_prompts_for_commands() {
    let mode = PermissionMode::AutoEdit;
    assert_eq!(
        permission(&mode, "edit_file", ToolPermission::Confirm),
        ToolPermission::Allow
    );
    assert_eq!(
        permission(&mode, "run_command", ToolPermission::Allow),
        ToolPermission::Confirm
    );
    assert_eq!(
        permission(&mode, "read_file", ToolPermission::Allow),
        ToolPermission::Allow
    );
}

#[test]
fn plan_refuses_mutation_without_hiding_reads() {
    let mode = PermissionMode::Plan;
    assert_eq!(
        permission(&mode, "edit_file", ToolPermission::Confirm),
        ToolPermission::Deny
    );
    assert_eq!(
        permission(&mode, "run_command", ToolPermission::Allow),
        ToolPermission::Deny
    );
    assert_eq!(
        permission(&mode, "read_file", ToolPermission::Allow),
        ToolPermission::Allow
    );
}

#[test]
fn unattended_never_prompts_and_applies_exact_grants() {
    let denied = PermissionMode::Unattended(UnattendedGrants::default());
    assert_eq!(
        permission(&denied, "edit_file", ToolPermission::Confirm),
        ToolPermission::Deny
    );
    assert_eq!(
        permission(&denied, "run_command", ToolPermission::Allow),
        ToolPermission::Deny
    );

    let granted = PermissionMode::Unattended(UnattendedGrants {
        edit_file: true,
        run_command_patterns: vec!["cargo *".into()],
    });
    assert_eq!(
        permission(&granted, "edit_file", ToolPermission::Confirm),
        ToolPermission::Allow
    );
    assert_eq!(
        permission(&granted, "run_command", ToolPermission::Allow),
        ToolPermission::Allow
    );
    assert_eq!(
        permission(&granted, "future_tool", ToolPermission::Confirm),
        ToolPermission::Deny
    );
}

/// `create_file` is a mutating file tool and rides the `edit_file` gate in
/// every mode: it confirms, runs, is refused, and is granted by the one
/// `--allow-edit` grant exactly as `edit_file` is. There is no creation
/// flag and no creation exemption.
#[test]
fn create_file_is_gated_exactly_like_edit_file_in_every_mode() {
    let granted = PermissionMode::Unattended(UnattendedGrants {
        edit_file: true,
        run_command_patterns: Vec::new(),
    });
    let modes = [
        (PermissionMode::Confirm, ToolPermission::Confirm),
        (PermissionMode::AutoEdit, ToolPermission::Allow),
        (PermissionMode::Plan, ToolPermission::Deny),
        (
            PermissionMode::Unattended(UnattendedGrants::default()),
            ToolPermission::Deny,
        ),
        (granted, ToolPermission::Allow),
    ];
    for (mode, expected) in modes {
        assert_eq!(
            permission(&mode, "create_file", ToolPermission::Confirm),
            expected,
            "{} must resolve create_file to {expected:?}",
            mode.label()
        );
        assert_eq!(
            permission(&mode, "create_file", ToolPermission::Confirm),
            permission(&mode, "edit_file", ToolPermission::Confirm),
            "{} must gate create_file and edit_file identically",
            mode.label()
        );
    }
}
