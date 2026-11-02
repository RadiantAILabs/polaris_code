//! Startup configuration resolved before CLI parsing.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::{CommandFactory, FromArgMatches, ValueEnum};
use polaris_code::agents::{PermissionCeiling, PermissionMode};
use polaris_code::project_trust::{
    MAX_FILE_BYTES, ProjectTrust, STORE_FILE, Subject, read_content,
};
use polaris_cosmos::contracts::ContentDigest;
use toml_edit::{DocumentMut, Item, TableLike};

#[path = "terminal/status_command.rs"]
mod status_command;

/// Resolved launch inputs and the files that supplied them.
///
/// Credential values remain private and are never included in diagnostic output.
#[derive(Default)]
pub(crate) struct Startup {
    files: Vec<PathBuf>,
    config_directory: Option<PathBuf>,
    config_dir_explicit: bool,
    profile: Option<String>,
    dotenv: Option<PathBuf>,
    settings: Settings,
    trust: ProjectTrust,
    notices: Vec<String>,
}

impl Startup {
    /// The user's trust decisions for the project being opened.
    pub(crate) fn trust(&self) -> &ProjectTrust {
        &self.trust
    }

    /// Project settings that were ignored, and how to apply them.
    pub(crate) fn notices(&self) -> &[String] {
        &self.notices
    }

    /// The model catalog with the configuration files applied: the environment
    /// wins, then the files, then `fallback`.
    pub(crate) fn catalog(
        &self,
        fallback: polaris_code::config::ModelCatalog,
    ) -> polaris_code::config::ModelCatalog {
        let lookup = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| {
                    let key = KEYS.iter().find(|key| key.env == name)?;
                    self.settings
                        .get(key.name)
                        .map(|setting| setting.value.clone())
                })
        };
        let builtin = fallback
            .models()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        polaris_code::config::ModelCatalog::from_lookup(fallback.default_model(), &builtin, &lookup)
    }

    pub(crate) fn register_status(
        self,
        commands: &polaris_code::terminal::CommandRegistry,
        settings: &polaris_code::terminal::SessionSettings,
        navigation: &polaris_code::terminal::SessionNavigation,
        workspace: &Path,
        report: polaris_code::providers::ProviderReport,
    ) -> Result<(), polaris_code::terminal::CommandError> {
        status_command::register(commands, settings, navigation, workspace, self, report)
    }
}

#[derive(Clone, Debug)]
struct Setting {
    value: String,
    path: PathBuf,
    key: String,
}

type Settings = BTreeMap<String, Setting>;

/// A setting `config.toml` accepts.
struct Key {
    /// The name in a configuration file.
    name: &'static str,
    /// The environment variable that overrides the file.
    env: &'static str,
    /// Whether a command-line flag of the same name overrides both, with the
    /// file supplying that flag's default. Without one, the value is read
    /// straight from the environment.
    flag: bool,
}

impl Key {
    const fn with_flag(name: &'static str, env: &'static str) -> Self {
        Self {
            name,
            env,
            flag: true,
        }
    }

    const fn env_only(name: &'static str, env: &'static str) -> Self {
        Self {
            name,
            env,
            flag: false,
        }
    }
}

/// The accepted settings: the CLI-only keys and direct environment reads share
/// this one precedence map.
const KEYS: &[Key] = &[
    Key::with_flag("provider", "POLARIS_CODE_PROVIDER"),
    Key::with_flag("model", "POLARIS_CODE_MODEL"),
    Key::env_only("models", "POLARIS_CODE_MODELS"),
    Key::with_flag("region", "POLARIS_CODE_REGION"),
    Key::with_flag("permission_mode", "POLARIS_CODE_PERMISSION_MODE"),
    Key::with_flag("permission_ceiling", "POLARIS_CODE_PERMISSION_CEILING"),
    Key::with_flag("max_cost_usd", "POLARIS_CODE_MAX_COST_USD"),
    Key::with_flag("max_tokens", "POLARIS_CODE_MAX_TOKENS"),
    Key::with_flag("max_turns", "POLARIS_CODE_MAX_TURNS"),
    Key::with_flag("max_output_tokens", "POLARIS_CODE_MAX_OUTPUT_TOKENS"),
];

fn command() -> clap::Command {
    super::Cli::command().arg(
        clap::Arg::new("profile")
            .long("profile")
            .value_name("NAME")
            .help("Select a named profile from the user and project configuration files"),
    )
}

/// Resolve launch inputs through the existing CLI and limit validators.
pub(crate) fn load() -> (super::Cli, Startup) {
    let cwd = std::env::current_dir().unwrap_or_else(|err| fail(err));
    let path = cwd.join(".env");
    let dotenv = match read_dotenv(&cwd).unwrap_or_else(|err| fail(err)) {
        Some(file) => {
            let is_set = |key: &str| std::env::var_os(key).is_some();
            // The launch directory is project-controlled. Refuse before applying
            // anything, so the file cannot choose where the user's own
            // configuration, credentials and trust decisions are read from.
            if let Some(key) = reserved_key(&file.entries, is_set) {
                fail(format!(
                    "{}: the environment file may not set {key}; set it in your shell or in \
                     your user configuration instead",
                    path.display()
                ));
            }
            // Apply the bytes that were checked, not a second read of the file.
            dotenvy::from_read(file.bytes.as_slice())
                .unwrap_or_else(|_| fail(format!("{}: invalid environment file", path.display())));
            eprintln!("Loaded environment file {}", path.display());
            Some(path)
        }
        None => None,
    };
    let (cli, mut startup) = load_from(
        std::env::args_os().collect(),
        std::env::vars_os().collect(),
        user_config_path().ok(),
    )
    .unwrap_or_else(|err| fail(err));
    startup.dotenv = dotenv;
    (cli, startup)
}

/// Variables a project's `.env` may not set: they choose where user-owned
/// configuration lives or what permissions a session opens with, so only the
/// user may set them.
const RESERVED_ENVIRONMENT: &[&str] = &[
    "POLARIS_CONFIG_DIR",
    "XDG_CONFIG_HOME",
    "HOME",
    "POLARIS_CODE_PERMISSION_MODE",
    "POLARIS_CODE_PERMISSION_CEILING",
];

/// A reserved variable the file would set. One the process already has is not
/// set by the file (an existing value wins), so it is not a problem.
fn reserved_key(entries: &BTreeMap<String, String>, is_set: impl Fn(&str) -> bool) -> Option<&str> {
    entries
        .keys()
        .map(String::as_str)
        .find(|key| RESERVED_ENVIRONMENT.contains(key) && !is_set(key))
}

fn load_from(
    argv: Vec<OsString>,
    environment: BTreeMap<OsString, OsString>,
    user: Option<PathBuf>,
) -> Result<(super::Cli, Startup), String> {
    let preliminary = command()
        .try_get_matches_from(argv.clone())
        .unwrap_or_else(|err| err.exit());
    let workspace = preliminary
        .get_one::<PathBuf>("working_dir")
        .cloned()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|err| fail(err)));
    let workspace = workspace
        .canonicalize()
        .map_err(|err| format!("{}: {err}", workspace.display()))?;
    let profile = preliminary.get_one::<String>("profile").cloned();
    let config_directory = user
        .as_ref()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    let trust = ProjectTrust::new(
        config_directory.as_ref().map(|dir| dir.join(STORE_FILE)),
        &workspace,
    );
    let mut startup = Startup {
        profile: profile.clone(),
        config_directory,
        config_dir_explicit: environment
            .get(std::ffi::OsStr::new(
                polaris_code::config_dir::CONFIG_DIR_VAR,
            ))
            .is_some_and(|value| !value.is_empty()),
        trust: trust.clone(),
        ..Startup::default()
    };
    let mut found_profile = false;
    // The user's own settings bound what the project may ask for.
    if let Some(path) = user
        && let Some(file) = read_config(&path, profile.as_deref())?
    {
        startup.files.push(path);
        startup.settings.extend(file.base);
        if let Some(overlay) = file.profile {
            found_profile = true;
            startup.settings.extend(overlay);
        }
    }
    let user_settings = startup.settings.clone();
    let project = workspace.join("polaris.toml");
    if let Some(file) = read_config(&project, profile.as_deref())? {
        trust.observe(Subject::Settings, file.digest);
        startup.files.push(project);
        found_profile |= file.profile.is_some();
        let mut layers = vec![file.base];
        layers.extend(file.profile);
        let asks = layers
            .iter()
            .any(|layer| !loosening(&user_settings, layer).is_empty());
        if asks && !trust.is_trusted(&Subject::Settings, file.digest) {
            for layer in &mut layers {
                for key in loosening(&user_settings, layer) {
                    if let Some(setting) = layer.remove(key) {
                        startup.notices.push(format!(
                            "{}: key `{}` = `{}` is more permissive than your own settings and \
                             was ignored",
                            setting.path.display(),
                            setting.key,
                            setting.value
                        ));
                    }
                }
            }
            startup.notices.push(
                "Review the project file, then run `/permissions trust-project` in a session and \
                 restart to apply it."
                    .to_string(),
            );
        }
        for layer in layers {
            startup.settings.extend(layer);
        }
    }
    if let Some(profile) = &profile
        && !found_profile
    {
        return Err(format!(
            "profile `{profile}` was not found in the user or project configuration"
        ));
    }
    let resolved = resolve(&startup.settings, &environment, &preliminary);
    let mut command = command();
    for key in KEYS {
        let Some(setting) = resolved.get(key.name) else {
            continue;
        };
        validate(key.name, &setting.value)
            .map_err(|err| format!("{}: key `{}`: {err}", setting.path.display(), setting.key))?;
        if key.flag {
            command = command.mut_arg(key.name, |arg| arg.default_value(setting.value.clone()));
        }
    }
    let mut matches = command
        .try_get_matches_from(argv)
        .unwrap_or_else(|err| err.exit());
    matches.remove_one::<String>("profile");
    let cli = super::Cli::from_arg_matches(&matches).unwrap_or_else(|err| err.exit());
    Ok((cli, startup))
}

fn fail(error: impl std::fmt::Display) -> ! {
    eprintln!("Configuration error: {error}");
    std::process::exit(2)
}

/// Why a user setting could not be saved.
#[derive(Debug)]
pub(crate) enum SettingsError {
    /// No user configuration directory could be located.
    NoConfigDir,
    /// The key is not a startup setting.
    UnknownKey {
        /// File that would have held the setting.
        path: PathBuf,
        /// The rejected key.
        key: String,
    },
    /// The value is not valid for the key.
    InvalidValue {
        /// File that would have held the setting.
        path: PathBuf,
        /// The key whose value was rejected.
        key: String,
        /// What is wrong with the value.
        reason: String,
    },
    /// The destination is a symlink, which is never replaced.
    Symlink(PathBuf),
    /// The existing file cannot be used; the text names the file and the problem.
    Existing(String),
    /// The file could not be written.
    Io {
        /// File or directory being written.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoConfigDir => f.write_str(
                "cannot locate user configuration: set POLARIS_CONFIG_DIR, XDG_CONFIG_HOME or HOME",
            ),
            Self::UnknownKey { path, key } => {
                write!(f, "{}: key `{key}`: unknown setting", path.display())
            }
            Self::InvalidValue { path, key, reason } => {
                write!(f, "{}: key `{key}`: {reason}", path.display())
            }
            Self::Symlink(path) => write!(
                f,
                "{}: refusing to replace a configuration symlink",
                path.display()
            ),
            Self::Existing(text) => f.write_str(text),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for SettingsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// For callers that report failures as text.
impl From<SettingsError> for String {
    fn from(error: SettingsError) -> Self {
        error.to_string()
    }
}

/// Resolve the user configuration file in the shared configuration directory.
pub(crate) fn user_config_path() -> Result<PathBuf, SettingsError> {
    polaris_code::config_dir::config_file("config.toml").ok_or(SettingsError::NoConfigDir)
}

/// Persist one validated user setting, preserving unrelated TOML content.
pub(crate) fn set_user_value(key: &str, value: &str) -> Result<(), SettingsError> {
    set_value(&user_config_path()?, key, value)
}

fn set_value(path: &Path, key: &str, value: &str) -> Result<(), SettingsError> {
    use std::io::Write;

    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| SettingsError::Io { path, source }
    };
    if !KEYS.iter().any(|known| known.name == key) {
        return Err(SettingsError::UnknownKey {
            path: path.to_path_buf(),
            key: key.to_string(),
        });
    }
    validate(key, value).map_err(|reason| SettingsError::InvalidValue {
        path: path.to_path_buf(),
        key: key.to_string(),
        reason,
    })?;
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(SettingsError::Symlink(path.to_path_buf()));
    }
    // Refuse to rewrite malformed or unsupported existing configuration.
    let existing = read_text(path).map_err(SettingsError::Existing)?;
    if let Some((text, _)) = &existing {
        parse_config(path, text, None).map_err(SettingsError::Existing)?;
    }
    let mut document = match &existing {
        Some((text, _)) => text
            .parse::<DocumentMut>()
            .map_err(|err| SettingsError::Existing(format!("{}: {err}", path.display())))?,
        None => DocumentMut::new(),
    };
    let mut replacement = toml_edit::Value::from(value);
    if let Some(old) = document.get(key).and_then(Item::as_value) {
        *replacement.decor_mut() = old.decor().clone();
    }
    document[key] = Item::Value(replacement);
    let parent = path.parent().ok_or_else(|| SettingsError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::other("configuration path has no parent"),
    })?;
    std::fs::create_dir_all(parent).map_err(io(parent))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(io(path))?;
    temporary
        .write_all(document.to_string().as_bytes())
        .map_err(io(path))?;
    temporary.as_file().sync_all().map_err(io(path))?;
    temporary.persist(path).map_err(|err| io(path)(err.error))?;
    Ok(())
}

fn resolve(
    settings: &Settings,
    env: &BTreeMap<OsString, OsString>,
    args: &clap::ArgMatches,
) -> Settings {
    let mut resolved = settings.clone();
    for key in KEYS {
        // The legacy cost env var configures per-task spend, independently
        // of the file key and flag's whole-run limit.
        if key.name != "max_cost_usd"
            && let Some(value) = env.get(std::ffi::OsStr::new(key.env))
            && (!matches!(key.name, "model" | "models")
                || !value.to_string_lossy().trim().is_empty())
        {
            resolved.insert(
                key.name.to_string(),
                Setting {
                    value: value.to_string_lossy().into_owned(),
                    path: PathBuf::from("environment"),
                    key: key.env.to_string(),
                },
            );
        }
        if key.flag && args.value_source(key.name) == Some(clap::parser::ValueSource::CommandLine) {
            resolved.remove(key.name);
        }
    }
    resolved
}

/// Settings that would make the session more permissive than `user` allows.
///
/// `user` holds the user's own settings. A project may always be stricter: a
/// lower mode, or a lower ceiling than the user's. It is more permissive when
/// its mode is above what the user's mode and ceiling admit (the built-in
/// default mode applies when the user names none), or when its ceiling is above
/// the user's. With no user ceiling, any ceiling the project names only
/// restricts.
fn loosening(user: &Settings, project: &Settings) -> Vec<&'static str> {
    let mode = |settings: &Settings, key: &str| {
        settings.get(key).and_then(|setting| {
            super::InteractivePermissionMode::from_str(&setting.value, true)
                .ok()
                .map(PermissionMode::from)
        })
    };
    let user_ceiling = mode(user, "permission_ceiling").and_then(PermissionCeiling::new);
    let user_mode = mode(user, "permission_mode").unwrap_or_default();
    let user_mode = match &user_ceiling {
        Some(ceiling) => ceiling.clamp(user_mode),
        None => user_mode,
    };
    let mut keys = Vec::new();
    if let Some(requested) = mode(project, "permission_mode")
        && !PermissionCeiling::new(user_mode).is_some_and(|bound| bound.admits(&requested))
    {
        keys.push("permission_mode");
    }
    if let Some(requested) = mode(project, "permission_ceiling")
        && user_ceiling
            .as_ref()
            .is_some_and(|bound| !bound.admits(&requested))
    {
        keys.push("permission_ceiling");
    }
    keys
}

/// A launch directory's `.env`, read once so what is checked is what is applied.
struct DotEnv {
    bytes: Vec<u8>,
    entries: BTreeMap<String, String>,
}

fn read_dotenv(workspace: &Path) -> Result<Option<DotEnv>, String> {
    let path = workspace.join(".env");
    let Some(content) = read_content(&path, MAX_FILE_BYTES)
        .map_err(|err| format!("{}: cannot read environment file ({err})", path.display()))?
    else {
        return Ok(None);
    };
    let mut entries = BTreeMap::new();
    for entry in dotenvy::from_read_iter(content.bytes.as_slice()) {
        let (key, value) =
            entry.map_err(|_| format!("{}: invalid environment entry", path.display()))?;
        entries.entry(key).or_insert(value);
    }
    Ok(Some(DotEnv {
        bytes: content.bytes,
        entries,
    }))
}

/// One configuration file's base settings, the selected profile's overlay, and
/// the digest of the bytes they were parsed from.
struct ConfigFile {
    base: Settings,
    profile: Option<Settings>,
    digest: ContentDigest,
}

fn read_text(path: &Path) -> Result<Option<(String, ContentDigest)>, String> {
    let Some(content) = read_content(path, MAX_FILE_BYTES).map_err(|err| err.to_string())? else {
        return Ok(None);
    };
    let text = String::from_utf8(content.bytes)
        .map_err(|_| format!("{}: not valid UTF-8", path.display()))?;
    Ok(Some((text, content.digest)))
}

fn read_config(path: &Path, profile: Option<&str>) -> Result<Option<ConfigFile>, String> {
    let Some((text, digest)) = read_text(path)? else {
        return Ok(None);
    };
    let (base, profile) = parse_config(path, &text, profile)?;
    Ok(Some(ConfigFile {
        base,
        profile,
        digest,
    }))
}

fn parse_config(
    path: &Path,
    text: &str,
    profile: Option<&str>,
) -> Result<(Settings, Option<Settings>), String> {
    let doc = text
        .parse::<DocumentMut>()
        .map_err(|err| format!("{}: {err}", path.display()))?;
    let base = read_table(path, "", doc.as_table(), true)?;
    let mut overlay = None;
    if let Some(profiles) = doc.get("profiles") {
        let profiles = profiles
            .as_table_like()
            .ok_or_else(|| format!("{}: key `profiles`: expected a table", path.display()))?;
        for (name, table) in profiles.iter() {
            let table = table.as_table_like().ok_or_else(|| {
                format!(
                    "{}: key `profiles.{name}`: expected a table",
                    path.display()
                )
            })?;
            let parsed = read_table(path, &format!("profiles.{name}."), table, false)?;
            if profile == Some(name) {
                overlay = Some(parsed);
            }
        }
    }
    Ok((base, overlay))
}

fn read_table(
    path: &Path,
    prefix: &str,
    table: &dyn TableLike,
    root: bool,
) -> Result<Settings, String> {
    let mut settings = Settings::new();
    for (key, item) in table.iter() {
        if root && key == "profiles" {
            continue;
        }
        let full_key = format!("{prefix}{key}");
        let bad = |why: &str| format!("{}: key `{full_key}`: {why}", path.display());
        if !KEYS.iter().any(|known| known.name == key) {
            return Err(bad("unknown setting"));
        }
        let value = setting_text(key, item).ok_or_else(|| {
            bad("expected a string or numeric setting (models also accepts an array of strings)")
        })?;
        validate(key, &value).map_err(|err| bad(&err))?;
        settings.insert(
            key.to_string(),
            Setting {
                value,
                path: path.to_path_buf(),
                key: full_key,
            },
        );
    }
    Ok(settings)
}

fn setting_text(key: &str, item: &Item) -> Option<String> {
    if key == "models"
        && let Some(array) = item.as_array()
    {
        return array
            .iter()
            .map(|value| value.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .map(|values| values.join(","));
    }
    if let Some(value) = item.as_str() {
        return Some(value.to_string());
    }
    if matches!(
        key,
        "max_cost_usd" | "max_tokens" | "max_turns" | "max_output_tokens"
    ) {
        return item
            .as_integer()
            .map(|value| value.to_string())
            .or_else(|| item.as_float().map(|value| value.to_string()));
    }
    None
}

fn validate(key: &str, value: &str) -> Result<(), String> {
    match key {
        "max_turns" => super::resolve_max_turns(Some(value)).map(|_| ()),
        "max_output_tokens" => super::resolve_max_output_tokens(Some(value)).map(|_| ()),
        "max_cost_usd" => {
            let value = value.parse::<f64>().map_err(|err| err.to_string())?;
            polaris_code::usage::RunCeiling::from_limits(Some(value), None)
                .map(|_| ())
                .map_err(|err| err.to_string())
        }
        "max_tokens" => {
            let value = value.parse::<u64>().map_err(|err| err.to_string())?;
            polaris_code::usage::RunCeiling::from_limits(None, Some(value))
                .map(|_| ())
                .map_err(|err| err.to_string())
        }
        "provider" | "permission_mode" | "permission_ceiling" => {
            // Reuse clap's parser, including provider variants contributed by the host.
            let flag = key.replace('_', "-");
            super::Cli::command()
                .try_get_matches_from(["polaris", &format!("--{flag}={value}")])
                .map(|_| ())
                .map_err(|err| err.to_string())
        }
        _ => {
            if value.trim().is_empty() {
                Err("value must not be empty".to_string())
            } else {
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: tempfile::TempDir,
        workspace: PathBuf,
        user: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let workspace = root.path().join("workspace");
            let user = root.path().join("user/polaris");
            std::fs::create_dir_all(&workspace).unwrap();
            std::fs::create_dir_all(&user).unwrap();
            Self {
                root,
                workspace,
                user,
            }
        }

        fn user(&self, text: &str) {
            std::fs::write(self.user.join("config.toml"), text).unwrap();
        }

        fn project(&self, text: &str) {
            std::fs::write(self.workspace.join("polaris.toml"), text).unwrap();
        }

        fn load(
            &self,
            flags: &[&str],
            environment: &[(&str, &str)],
        ) -> Result<(super::super::Cli, Startup), String> {
            let mut argv = vec!["polaris".into(), self.workspace.clone().into_os_string()];
            argv.extend(flags.iter().map(OsString::from));
            let mut env = BTreeMap::from([(
                "XDG_CONFIG_HOME".into(),
                self.user.parent().unwrap().as_os_str().into(),
            )]);
            env.extend(
                environment
                    .iter()
                    .map(|(key, value)| (key.into(), value.into())),
            );
            let directory = polaris_code::config_dir::resolve(
                env.get(std::ffi::OsStr::new("POLARIS_CONFIG_DIR"))
                    .map(PathBuf::from),
                env.get(std::ffi::OsStr::new("XDG_CONFIG_HOME"))
                    .map(PathBuf::from),
                env.get(std::ffi::OsStr::new("HOME")).map(PathBuf::from),
            );
            load_from(argv, env, directory.map(|path| path.join("config.toml")))
        }
    }

    #[test]
    fn configuration_precedence_is_flags_environment_project_user_defaults() {
        let fixture = Fixture::new();
        let (cli, startup) = fixture.load(&[], &[]).unwrap();
        assert!(cli.max_tokens.is_none());
        assert!(startup.files.is_empty());
        fixture.user("max_tokens = 100\nregion = 'user-region'\n");
        let (cli, _) = fixture.load(&[], &[]).unwrap();
        assert_eq!(cli.max_tokens, Some(100));
        fixture.project("max_tokens = 200\n");
        let (cli, startup) = fixture.load(&[], &[]).unwrap();
        assert_eq!(cli.max_tokens, Some(200));
        assert_eq!(cli.region.as_deref(), Some("user-region"));
        assert_eq!(
            startup.files,
            [
                fixture.user.join("config.toml"),
                fixture.workspace.join("polaris.toml")
            ]
        );
        let (cli, _) = fixture
            .load(&[], &[("POLARIS_CODE_MAX_TOKENS", "300")])
            .unwrap();
        assert_eq!(cli.max_tokens, Some(300));
        let (cli, _) = fixture
            .load(&["--max-tokens=400"], &[("POLARIS_CODE_MAX_TOKENS", "300")])
            .unwrap();
        assert_eq!(cli.max_tokens, Some(400));
    }

    #[test]
    fn profiles_overlay_each_file_before_project_overrides_user() {
        let fixture = Fixture::new();
        fixture.user("max_turns = 10\nregion = 'user'\n[profiles.careful]\nmax_turns = 20\npermission_mode = 'plan'\n");
        fixture.project("max_turns = 30\n[profiles.careful]\nmax_turns = 40\n");
        let (cli, startup) = fixture.load(&["--profile=careful"], &[]).unwrap();
        assert_eq!(cli.max_turns, Some(40));
        assert!(matches!(
            cli.permission_mode,
            Some(super::super::InteractivePermissionMode::Plan)
        ));
        assert_eq!(startup.profile.as_deref(), Some("careful"));
        let (cli, _) = fixture.load(&[], &[]).unwrap();
        assert_eq!(cli.max_turns, Some(30));
        assert!(cli.permission_mode.is_none());
        assert!(
            fixture
                .load(&["--profile=missing"], &[])
                .err()
                .unwrap()
                .contains("profile `missing`")
        );
    }

    #[test]
    fn each_supported_setting_reaches_its_existing_consumer() {
        let fixture = Fixture::new();
        fixture.project("provider = 'bedrock'\nmodel = 'bedrock/custom'\nmodels = ['bedrock/other', 'bedrock/custom']\nregion = 'us-test-1'\npermission_mode = 'plan'\npermission_ceiling = 'confirm'\nmax_cost_usd = 1.5\nmax_tokens = 2500\nmax_turns = 7\nmax_output_tokens = 1200\n");
        let (cli, startup) = fixture.load(&[], &[]).unwrap();
        assert_eq!(cli.provider, super::super::Provider::Bedrock);
        assert_eq!(cli.model.as_deref(), Some("bedrock/custom"));
        assert_eq!(cli.region.as_deref(), Some("us-test-1"));
        assert_eq!(cli.max_cost_usd, Some(1.5));
        assert_eq!(cli.max_tokens, Some(2500));
        assert_eq!(cli.max_turns, Some(7));
        assert_eq!(cli.max_output_tokens.unwrap().get(), 1200);
        assert!(matches!(
            cli.permission_ceiling,
            Some(super::super::InteractivePermissionMode::Confirm)
        ));
        assert_eq!(
            startup.settings["models"].value,
            "bedrock/other,bedrock/custom"
        );
    }

    #[test]
    fn env_and_flags_override_file_permissions_and_limits() {
        let fixture = Fixture::new();
        fixture.project("permission_mode = 'plan'\nmax_turns = 9\nmax_output_tokens = 500\n");
        let (cli, _) = fixture
            .load(
                &["--permission-mode=auto-edit", "--max-turns=11"],
                &[
                    ("POLARIS_CODE_PERMISSION_MODE", "confirm"),
                    ("POLARIS_CODE_MAX_TURNS", "10"),
                    ("POLARIS_CODE_MAX_OUTPUT_TOKENS", "600"),
                ],
            )
            .unwrap();
        assert!(matches!(
            cli.permission_mode,
            Some(super::super::InteractivePermissionMode::AutoEdit)
        ));
        assert_eq!(cli.max_turns, Some(11));
        assert_eq!(cli.max_output_tokens.unwrap().get(), 600);
    }

    #[test]
    fn blank_model_environment_keeps_the_existing_unset_semantics() {
        let fixture = Fixture::new();
        fixture.project("model = 'fake/default'\nmodels = ['fake/default']\n");
        let (cli, _) = fixture
            .load(
                &[],
                &[("POLARIS_CODE_MODEL", " "), ("POLARIS_CODE_MODELS", " ")],
            )
            .unwrap();
        assert_eq!(cli.model.as_deref(), Some("fake/default"));
    }

    #[test]
    fn task_cost_environment_does_not_redefine_whole_run_cost() {
        let fixture = Fixture::new();
        fixture.project("max_cost_usd = 3.0\n");
        let (cli, _) = fixture
            .load(&[], &[("POLARIS_CODE_MAX_COST_USD", "0")])
            .unwrap();
        assert_eq!(cli.max_cost_usd, Some(3.0));
    }

    #[test]
    fn unknown_keys_name_the_file_and_full_key() {
        let fixture = Fixture::new();
        for (text, key) in [
            ("max_truns = 9", "max_truns"),
            (
                "[profiles.careful]\nmax_truns = 9",
                "profiles.careful.max_truns",
            ),
            ("[permissions]\nmode = 'plan'", "permissions"),
        ] {
            fixture.project(text);
            let error = fixture.load(&[], &[]).err().unwrap();
            assert!(
                error.contains(&fixture.workspace.join("polaris.toml").display().to_string()),
                "{error}"
            );
            assert!(error.contains(&format!("key `{key}`")), "{error}");
        }
    }

    #[test]
    fn invalid_values_name_the_file_and_key_even_when_shadowed() {
        let fixture = Fixture::new();
        for (key, value) in [
            ("max_turns", "0"),
            ("max_output_tokens", "0"),
            ("max_tokens", "0"),
            ("max_cost_usd", "nan"),
            ("provider", "'unknown'"),
            ("permission_mode", "'allow-all'"),
            ("permission_ceiling", "true"),
            ("models", "['ok', 3]"),
        ] {
            fixture.project(&format!("{key} = {value}"));
            let error = fixture.load(&["--max-turns=12"], &[]).err().unwrap();
            assert!(error.contains("polaris.toml"), "{error}");
            assert!(error.contains(&format!("key `{key}`")), "{error}");
        }
    }

    #[test]
    fn dotenv_only_reads_the_supplied_launch_folder() {
        let fixture = Fixture::new();
        std::fs::write(
            fixture.root.path().join(".env"),
            "PARENT_ONLY=private-parent",
        )
        .unwrap();
        assert!(read_dotenv(&fixture.workspace).unwrap().is_none());
        std::fs::write(
            fixture.workspace.join(".env"),
            "LOCAL_ONLY=local\nLOCAL_ONLY=second",
        )
        .unwrap();
        let local = read_dotenv(&fixture.workspace).unwrap().unwrap().entries;
        assert_eq!(local.get("LOCAL_ONLY").map(String::as_str), Some("local"));
        assert!(!local.contains_key("PARENT_ONLY"));
    }

    #[test]
    fn dotenv_errors_do_not_echo_secret_values() {
        let fixture = Fixture::new();
        std::fs::write(
            fixture.workspace.join(".env"),
            "SECRET='private-malformed-value",
        )
        .unwrap();
        let error = read_dotenv(&fixture.workspace).err().unwrap();
        assert!(error.contains(".env"));
        assert!(!error.contains("private-malformed-value"));
    }

    #[test]
    fn reserved_environment_variables_are_refused_unless_already_set() {
        let entries = BTreeMap::from([
            ("OPENAI_API_KEY".to_string(), "x".to_string()),
            ("POLARIS_CONFIG_DIR".to_string(), "/tmp/hostile".to_string()),
        ]);
        assert_eq!(
            reserved_key(&entries, |_| false),
            Some("POLARIS_CONFIG_DIR")
        );
        // The process value wins over the file, so the file changes nothing.
        assert_eq!(
            reserved_key(&entries, |key| key == "POLARIS_CONFIG_DIR"),
            None
        );
        for key in RESERVED_ENVIRONMENT {
            let entries = BTreeMap::from([((*key).to_string(), "x".to_string())]);
            assert_eq!(reserved_key(&entries, |_| false), Some(*key));
        }
        let ordinary = BTreeMap::from([("OPENAI_API_KEY".to_string(), "x".to_string())]);
        assert_eq!(reserved_key(&ordinary, |_| false), None);
    }

    #[test]
    fn an_oversized_or_special_startup_file_is_refused() {
        let fixture = Fixture::new();
        let oversized = vec![b'#'; usize::try_from(MAX_FILE_BYTES).unwrap() + 1];
        std::fs::write(fixture.workspace.join("polaris.toml"), &oversized).unwrap();
        let error = fixture.load(&[], &[]).err().unwrap();
        assert!(error.contains("polaris.toml"), "{error}");
        assert!(error.contains("larger than"), "{error}");
        std::fs::remove_file(fixture.workspace.join("polaris.toml")).unwrap();
        std::fs::write(fixture.user.join("config.toml"), &oversized).unwrap();
        let error = fixture.load(&[], &[]).err().unwrap();
        assert!(error.contains("config.toml"), "{error}");
        assert!(error.contains("larger than"), "{error}");
        std::fs::remove_file(fixture.user.join("config.toml")).unwrap();
        std::fs::write(fixture.workspace.join(".env"), &oversized).unwrap();
        let error = read_dotenv(&fixture.workspace).err().unwrap();
        assert!(error.contains("larger than"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_device_or_pipe_is_never_read_as_a_startup_file() {
        let fixture = Fixture::new();
        let project = fixture.workspace.join("polaris.toml");
        // Reading this would never finish.
        std::os::unix::fs::symlink("/dev/zero", &project).unwrap();
        let error = fixture.load(&[], &[]).err().unwrap();
        assert!(error.contains("not a regular file"), "{error}");
        std::fs::remove_file(&project).unwrap();
        let user = fixture.user.join("config.toml");
        std::os::unix::fs::symlink("/dev/zero", &user).unwrap();
        let error = fixture.load(&[], &[]).err().unwrap();
        assert!(error.contains("not a regular file"), "{error}");
        std::fs::remove_file(&user).unwrap();
        // Reading a pipe with no writer would block.
        if std::process::Command::new("mkfifo")
            .arg(&project)
            .status()
            .is_ok_and(|status| status.success())
        {
            let error = fixture.load(&[], &[]).err().unwrap();
            assert!(error.contains("not a regular file"), "{error}");
        }
        std::os::unix::fs::symlink("/dev/zero", fixture.workspace.join(".env")).unwrap();
        let error = read_dotenv(&fixture.workspace).err().unwrap();
        assert!(error.contains("not a regular file"), "{error}");
    }

    /// Record the user's trust in the project's `polaris.toml` as it is now.
    fn trust_project_settings(fixture: &Fixture) {
        let root = fixture.workspace.canonicalize().unwrap();
        let digest = read_content(&fixture.workspace.join("polaris.toml"), MAX_FILE_BYTES)
            .unwrap()
            .unwrap()
            .digest;
        ProjectTrust::new(Some(fixture.user.join(STORE_FILE)), &root)
            .grant(&Subject::Settings, digest)
            .unwrap();
    }

    #[test]
    fn a_hostile_project_file_cannot_loosen_the_users_permissions() {
        let fixture = Fixture::new();
        fixture.user("permission_mode = 'plan'\npermission_ceiling = 'confirm'\n");
        fixture.project("permission_mode = 'auto'\npermission_ceiling = 'auto'\nmax_turns = 5\n");
        let (cli, startup) = fixture.load(&[], &[]).unwrap();
        assert!(matches!(
            cli.permission_mode,
            Some(super::super::InteractivePermissionMode::Plan)
        ));
        assert!(matches!(
            cli.permission_ceiling,
            Some(super::super::InteractivePermissionMode::Confirm)
        ));
        // Settings that are not about permissions still apply.
        assert_eq!(cli.max_turns, Some(5));
        let notices = startup.notices.join("\n");
        assert!(notices.contains("permission_mode"), "{notices}");
        assert!(notices.contains("permission_ceiling"), "{notices}");
        assert!(notices.contains("/permissions trust-project"), "{notices}");
    }

    #[test]
    fn a_hostile_profile_overlay_cannot_loosen_the_users_permissions() {
        let fixture = Fixture::new();
        fixture.user("permission_ceiling = 'confirm'\n");
        fixture.project(
            "[profiles.wide]\npermission_mode = 'auto-edit'\npermission_ceiling = 'auto'\n",
        );
        let (cli, startup) = fixture.load(&["--profile=wide"], &[]).unwrap();
        assert!(cli.permission_mode.is_none());
        assert!(matches!(
            cli.permission_ceiling,
            Some(super::super::InteractivePermissionMode::Confirm)
        ));
        assert!(!startup.notices.is_empty());
        // The same overlay is not read without its profile.
        let (_, startup) = fixture.load(&[], &[]).unwrap();
        assert!(startup.notices.is_empty());
    }

    #[test]
    fn a_project_may_be_stricter_than_the_user_without_trust() {
        let fixture = Fixture::new();
        fixture.user("permission_mode = 'auto-edit'\npermission_ceiling = 'auto-edit'\n");
        fixture.project("permission_mode = 'plan'\npermission_ceiling = 'confirm'\n");
        let (cli, startup) = fixture.load(&[], &[]).unwrap();
        assert!(matches!(
            cli.permission_mode,
            Some(super::super::InteractivePermissionMode::Plan)
        ));
        assert!(matches!(
            cli.permission_ceiling,
            Some(super::super::InteractivePermissionMode::Confirm)
        ));
        assert!(startup.notices.is_empty(), "{:?}", startup.notices);
    }

    #[test]
    fn a_project_without_a_user_baseline_is_held_to_the_default_mode() {
        let fixture = Fixture::new();
        fixture.project("permission_mode = 'auto-edit'\n");
        let (cli, startup) = fixture.load(&[], &[]).unwrap();
        assert!(cli.permission_mode.is_none());
        assert!(!startup.notices.is_empty());
        fixture.project("permission_mode = 'plan'\n");
        let (cli, _) = fixture.load(&[], &[]).unwrap();
        assert!(matches!(
            cli.permission_mode,
            Some(super::super::InteractivePermissionMode::Plan)
        ));
    }

    #[test]
    fn trusting_the_project_file_applies_its_looser_settings_until_it_changes() {
        let fixture = Fixture::new();
        fixture.user("permission_ceiling = 'confirm'\n");
        fixture.project("permission_ceiling = 'auto-edit'\n");
        let (cli, _) = fixture.load(&[], &[]).unwrap();
        assert!(matches!(
            cli.permission_ceiling,
            Some(super::super::InteractivePermissionMode::Confirm)
        ));
        trust_project_settings(&fixture);
        let (cli, startup) = fixture.load(&[], &[]).unwrap();
        assert!(matches!(
            cli.permission_ceiling,
            Some(super::super::InteractivePermissionMode::AutoEdit)
        ));
        assert!(startup.notices.is_empty(), "{:?}", startup.notices);
        // Any change to the file ends the trust that was given to the old text.
        fixture.project("permission_ceiling = 'auto-edit'\nmax_turns = 99\n");
        let (cli, startup) = fixture.load(&[], &[]).unwrap();
        assert!(matches!(
            cli.permission_ceiling,
            Some(super::super::InteractivePermissionMode::Confirm)
        ));
        assert!(!startup.notices.is_empty());
    }

    #[test]
    fn trust_for_one_project_does_not_cover_another() {
        let fixture = Fixture::new();
        fixture.user("permission_ceiling = 'confirm'\n");
        fixture.project("permission_ceiling = 'auto-edit'\n");
        trust_project_settings(&fixture);
        let other = fixture.root.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::copy(
            fixture.workspace.join("polaris.toml"),
            other.join("polaris.toml"),
        )
        .unwrap();
        let argv = vec!["polaris".into(), other.into_os_string()];
        let environment = BTreeMap::new();
        let (cli, _) =
            load_from(argv, environment, Some(fixture.user.join("config.toml"))).unwrap();
        assert!(matches!(
            cli.permission_ceiling,
            Some(super::super::InteractivePermissionMode::Confirm)
        ));
    }

    #[test]
    fn saving_a_user_setting_preserves_comments_profiles_and_other_values() {
        let fixture = Fixture::new();
        fixture.user("# Personal defaults\nprovider = 'anthropic' # backend\nmax_tokens = 500\n\n[profiles.inspect]\npermission_mode = 'plan'\n");
        let path = fixture.user.join("config.toml");
        set_value(&path, "provider", "bedrock").unwrap();
        set_value(&path, "region", "us-test-1").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# Personal defaults"));
        assert!(text.contains("# backend"));
        assert!(text.contains("max_tokens = 500"));
        assert!(text.contains("[profiles.inspect]"));
        let file = read_config(&path, Some("inspect")).unwrap().unwrap();
        assert_eq!(file.base["provider"].value, "bedrock");
        assert_eq!(file.base["region"].value, "us-test-1");
        assert_eq!(file.profile.unwrap()["permission_mode"].value, "plan");
    }

    #[test]
    fn saving_invalid_settings_leaves_the_original_file_intact() {
        let fixture = Fixture::new();
        fixture.user("provider = 'anthropic'\n");
        let path = fixture.user.join("config.toml");
        for (key, value) in [
            ("provider", "unknown"),
            ("api_key", "private-secret"),
            ("max_turns", "0"),
        ] {
            assert!(set_value(&path, key, value).is_err());
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "provider = 'anthropic'\n"
            );
        }
        let missing = fixture.root.path().join("new/polaris/config.toml");
        set_value(&missing, "provider", "bedrock").unwrap();
        assert_eq!(
            read_config(&missing, None).unwrap().unwrap().base["provider"].value,
            "bedrock"
        );
    }

    #[cfg(unix)]
    #[test]
    fn saving_refuses_to_replace_configuration_symlinks() {
        let fixture = Fixture::new();
        let target = fixture.root.path().join("target.toml");
        std::fs::write(&target, "provider = 'anthropic'\n").unwrap();
        let link = fixture.user.join("config.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(
            set_value(&link, "provider", "bedrock")
                .unwrap_err()
                .to_string()
                .contains("symlink")
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "provider = 'anthropic'\n"
        );
    }

    #[test]
    fn config_location_uses_explicit_directory_then_xdg_then_home() {
        use polaris_code::config_dir::resolve;

        for (explicit, xdg, home, expected) in [
            (
                Some("/work-setup"),
                Some("/custom"),
                Some("/home/test"),
                Some("/work-setup/config.toml"),
            ),
            (
                None,
                Some("/custom"),
                Some("/home/test"),
                Some("/custom/polaris/config.toml"),
            ),
            (
                None,
                None,
                Some("/home/test"),
                Some("/home/test/.config/polaris/config.toml"),
            ),
            (
                Some(""),
                Some(""),
                Some("/home/test"),
                Some("/home/test/.config/polaris/config.toml"),
            ),
            (None, None, None, None),
        ] {
            let file = resolve(
                explicit.map(PathBuf::from),
                xdg.map(PathBuf::from),
                home.map(PathBuf::from),
            )
            .map(|directory| directory.join("config.toml"));
            assert_eq!(file, expected.map(PathBuf::from));
        }
    }

    #[test]
    fn explicit_config_directory_selects_an_independent_setup() {
        let fixture = Fixture::new();
        fixture.user("max_tokens = 100\n");
        let custom = fixture.root.path().join("work-setup");
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::write(custom.join("config.toml"), "max_tokens = 200\n").unwrap();
        let (cli, startup) = fixture
            .load(&[], &[("POLARIS_CONFIG_DIR", custom.to_str().unwrap())])
            .unwrap();
        assert_eq!(cli.max_tokens, Some(200));
        assert_eq!(startup.files, [custom.join("config.toml")]);
        assert_eq!(startup.config_directory.as_deref(), Some(custom.as_path()));
        assert!(startup.config_dir_explicit);
        let (cli, startup) = fixture.load(&[], &[("POLARIS_CONFIG_DIR", "")]).unwrap();
        assert_eq!(cli.max_tokens, Some(100));
        assert_eq!(
            startup.config_directory.as_deref(),
            Some(fixture.user.as_path())
        );
        assert!(!startup.config_dir_explicit);
    }

    #[test]
    fn files_choose_the_default_model_and_the_declared_models() {
        use polaris_code::config::ModelCatalog;
        let fixture = Fixture::new();
        let fallback = || ModelCatalog::new("fallback/default", ["fallback/default", "fallback/b"]);

        let (_, startup) = fixture.load(&[], &[]).unwrap();
        let catalog = startup.catalog(fallback());
        assert_eq!(catalog.default_model(), "fallback/default");
        assert_eq!(catalog.models(), fallback().models());

        fixture.user("model = 'openai/mine'\nmodels = ['openai/mine', 'openai/other']\n");
        let (_, startup) = fixture.load(&[], &[]).unwrap();
        let catalog = startup.catalog(fallback());
        assert_eq!(catalog.default_model(), "openai/mine");
        assert_eq!(catalog.models(), ["openai/mine", "openai/other"]);
    }
}
