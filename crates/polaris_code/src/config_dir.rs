//! The user-level configuration directory.
//!
//! Everything `polaris` keeps per user, rather than per workspace, lives in one
//! directory: the config file and its profiles, stored credentials, permission
//! rules, and MCP server definitions. Per-workspace state stays in
//! [`crate::state::state_dir`].
//!
//! The directory is, in order:
//!
//! 1. `$POLARIS_CONFIG_DIR`, when set and not empty. Like `CLAUDE_CONFIG_DIR` or
//!    `CODEX_HOME`, this lets one user keep several independent setups — for
//!    example a work and a personal account — and pick one per launch.
//! 2. `$XDG_CONFIG_HOME/polaris`, when `XDG_CONFIG_HOME` is set and not empty.
//! 3. `$HOME/.config/polaris`.
//!
//! [`config_dir`] only resolves the path; callers that write create it.

use std::path::{Path, PathBuf};

/// The environment variable that selects the user configuration directory.
pub const CONFIG_DIR_VAR: &str = "POLARIS_CONFIG_DIR";

/// Resolves the user configuration directory from the process environment.
///
/// Returns `None` only when none of `POLARIS_CONFIG_DIR`, `XDG_CONFIG_HOME` or a
/// home directory is available.
pub fn config_dir() -> Option<PathBuf> {
    resolve(
        std::env::var_os(CONFIG_DIR_VAR).map(PathBuf::from),
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::home_dir(),
    )
}

/// Resolves the directory from explicit inputs; [`config_dir`] passes the
/// process environment. Empty values count as unset.
pub fn resolve(
    explicit: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    let set = |path: Option<PathBuf>| path.filter(|path| !path.as_os_str().is_empty());
    set(explicit)
        .or_else(|| set(xdg_config_home).map(|dir| dir.join("polaris")))
        .or_else(|| set(home).map(|dir| dir.join(".config").join("polaris")))
}

/// Joins `file` onto the user configuration directory.
pub fn config_file(file: impl AsRef<Path>) -> Option<PathBuf> {
    config_dir().map(|dir| dir.join(file))
}

#[cfg(test)]
mod tests {
    use super::resolve;
    use std::path::PathBuf;

    fn p(path: &str) -> Option<PathBuf> {
        Some(PathBuf::from(path))
    }

    #[test]
    fn explicit_directory_wins() {
        assert_eq!(resolve(p("/a"), p("/x"), p("/h")), p("/a"));
    }

    #[test]
    fn xdg_then_home() {
        assert_eq!(resolve(None, p("/x"), p("/h")), p("/x/polaris"));
        assert_eq!(resolve(None, None, p("/h")), p("/h/.config/polaris"));
    }

    #[test]
    fn empty_values_count_as_unset() {
        assert_eq!(resolve(p(""), p(""), p("/h")), p("/h/.config/polaris"));
        assert_eq!(resolve(None, None, None), None);
    }
}
