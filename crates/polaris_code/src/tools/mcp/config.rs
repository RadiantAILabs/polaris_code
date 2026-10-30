//! Bounded loading of project and user MCP server declarations.
//!
//! The user's own `mcp.json` is trusted. A project's `.mcp.json` is repository
//! content, so each of its servers starts only once the user has trusted that
//! exact declaration (see [`crate::project_trust`]).

use std::collections::BTreeMap;
use std::path::Path;

use polaris_cosmos::contracts::ContentDigest;
use serde::{Deserialize, Serialize};

use crate::project_trust::{MAX_FILE_BYTES, ProjectTrust, ReadError, Subject, read_content};

/// Most untrusted project servers listed; the rest are counted, not shown.
const MAX_WITHHELD: usize = 32;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ServerConfig {
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub url: Option<String>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub disabled: bool,
}

fn default_timeout() -> u64 {
    30
}

#[derive(Default, Deserialize)]
struct Config {
    #[serde(rename = "mcpServers", default)]
    servers: BTreeMap<String, ServerConfig>,
}

/// A project server that was not started because the user has not trusted it.
pub(super) struct Withheld {
    pub name: String,
    config: ServerConfig,
}

impl Withheld {
    /// What starting this server would do, for the user to review.
    ///
    /// Control characters are escaped and environment values are left out; only
    /// their names are shown.
    pub fn summary(&self) -> String {
        let quoted = |text: &String| format!("{text:?}");
        let config = &self.config;
        let mut summary = match (&config.command, &config.url) {
            (Some(command), _) => {
                let words = std::iter::once(command).chain(&config.args).map(quoted);
                format!("runs {}", words.collect::<Vec<_>>().join(" "))
            }
            (None, Some(url)) => format!("connects to {}", quoted(url)),
            (None, None) => "declares no command or url".to_string(),
        };
        if !config.env.is_empty() {
            let names = config.env.keys().map(quoted).collect::<Vec<_>>();
            summary.push_str(&format!("; sets environment {}", names.join(", ")));
        }
        summary.chars().take(400).collect()
    }
}

/// Declarations after deciding which may start.
#[derive(Default)]
pub(super) struct Loaded {
    /// Servers that may start: the user's own and the project's trusted ones.
    pub servers: BTreeMap<String, ServerConfig>,
    /// Project servers that were not started because they are not trusted.
    pub withheld: Vec<Withheld>,
    /// Problems and decisions worth showing, without credential values.
    pub errors: Vec<String>,
}

// Validate instead of rewriting names: normalization could silently alias two
// independent servers or tools, changing the target of an operator's grant.
pub(super) fn valid_component(name: &str) -> bool {
    !name.is_empty()
        && !name.contains("__")
        && !name.starts_with('_')
        && !name.ends_with('_')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

// Names come from files; do not display arbitrary ones in terminal text.
fn shown(name: &str) -> &str {
    if valid_component(name) {
        name
    } else {
        "(invalid server name)"
    }
}

fn read(path: &Path, root: &Path) -> Result<Config, String> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(_) => return Err(format!("Cannot read {}", path.display())),
        Ok(_) => {}
    }
    let resolved = path
        .canonicalize()
        .map_err(|_| format!("Cannot resolve {}", path.display()))?;
    let root = root.canonicalize().map_err(|_| {
        format!(
            "Cannot resolve configuration directory for {}",
            path.display()
        )
    })?;
    if !resolved.starts_with(root) {
        return Err(format!(
            "Configuration escapes its directory: {}",
            path.display()
        ));
    }
    let content = match read_content(&resolved, MAX_FILE_BYTES) {
        Ok(Some(content)) => content,
        Ok(None) => return Ok(Config::default()),
        Err(ReadError::TooLarge { .. }) => {
            return Err(format!("Configuration exceeds 1 MiB: {}", path.display()));
        }
        Err(ReadError::NotRegular(_)) => {
            return Err(format!("Not a regular file: {}", path.display()));
        }
        Err(_) => return Err(format!("Cannot read {}", path.display())),
    };
    // Never echo parser errors: they may contain an explicit credential value.
    serde_json::from_slice(&content.bytes)
        .map_err(|_| format!("Invalid MCP JSON in {}", path.display()))
}

/// The digest a trust decision for one declaration is bound to.
///
/// Fields serialize in a fixed order and `env` is a sorted map, so equal
/// declarations have equal bytes and any change to what would run changes it.
fn declaration_digest(config: &ServerConfig) -> Option<ContentDigest> {
    serde_json::to_vec(config)
        .ok()
        .map(|bytes| ContentDigest::sha256(&bytes))
}

/// Load the user's servers, then decide which of the project's may start.
///
/// A project server starts only when the user trusted its exact declaration.
/// It never replaces a server of the same name that the user declared, and its
/// `readOnly` claim is dropped: only the user's own configuration can say a
/// server is read-only.
pub(super) fn load(working_dir: &Path, user_path: Option<&Path>, trust: &ProjectTrust) -> Loaded {
    let mut loaded = Loaded::default();
    if let Some(path) = user_path
        && let Some(root) = path.parent()
    {
        match read(path, root) {
            Ok(config) => loaded.servers.extend(config.servers),
            Err(error) => loaded.errors.push(error),
        }
    }
    match read(&working_dir.join(".mcp.json"), working_dir) {
        Ok(config) => admit_project(&mut loaded, config, trust),
        Err(error) => loaded.errors.push(error),
    }
    loaded
}

fn admit_project(loaded: &mut Loaded, config: Config, trust: &ProjectTrust) {
    let mut hidden = 0_usize;
    for (name, mut server) in config.servers {
        if loaded.servers.contains_key(&name) {
            loaded.errors.push(format!(
                "Project MCP server `{}` ignored: your own configuration already defines a server \
                 with that name",
                shown(&name)
            ));
            continue;
        }
        if !valid_component(&name) || server.disabled {
            // Never started, so there is nothing to trust; the registry reports why.
            server.read_only = false;
            loaded.servers.insert(name, server);
            continue;
        }
        let digest = declaration_digest(&server);
        if server.read_only {
            server.read_only = false;
            loaded.errors.push(format!(
                "Project MCP server `{name}`: `readOnly` ignored; only your own configuration can \
                 mark a server read-only"
            ));
        }
        let subject = Subject::McpServer(name.clone());
        if let Some(digest) = digest {
            trust.observe(subject.clone(), digest);
            if trust.is_trusted(&subject, digest) {
                loaded.servers.insert(name, server);
                continue;
            }
        }
        if loaded.withheld.len() < MAX_WITHHELD {
            loaded.withheld.push(Withheld {
                name,
                config: server,
            });
        } else {
            hidden += 1;
        }
    }
    if hidden > 0 {
        loaded.errors.push(format!(
            "{hidden} more untrusted project MCP servers are not listed and were not started"
        ));
    }
}
