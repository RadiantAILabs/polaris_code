//! Credentials saved by `polaris login`.
//!
//! One TOML file in the user configuration directory ([`crate::config_dir`]),
//! readable by its owner only:
//!
//! ```toml
//! [anthropic]
//! api_key = "sk-ant-..."
//!
//! [openai]
//! api_key = "sk-..."
//!
//! [bedrock]
//! profile = "work"
//! ```
//!
//! A stored key is the last resort: an environment variable, which includes a
//! `.env` file in the launch folder, always wins (see [`crate::providers`]).
//! The file is written atomically, never through a symlink, and refused when it
//! is readable by anyone but its owner — a key that others can read is no
//! longer private, so using it silently would hide that.
//!
//! Nothing in this module prints a key. [`mask_key`] is the only way one is
//! ever shown, and it shows the last four characters at most.

use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table, value};

use crate::config_dir;
use crate::providers::Provider;

/// The file inside the user configuration directory that holds the credentials.
pub const CREDENTIALS_FILE: &str = "credentials.toml";

/// Longest key or profile name accepted, far beyond any real one.
const MAX_VALUE_LEN: usize = 4096;

/// Why a credential could not be read or saved.
#[derive(Debug)]
#[non_exhaustive]
pub enum CredentialError {
    /// The file or its directory could not be read or written.
    Io {
        /// The path involved.
        path: PathBuf,
        /// What the operating system said.
        source: std::io::Error,
    },
    /// The file exists but is not valid TOML.
    Malformed {
        /// The path involved.
        path: PathBuf,
        /// One-based line containing the parse error, when available.
        line: Option<usize>,
        /// One-based column containing the parse error, when available.
        column: Option<usize>,
    },
    /// The file can be read by users other than its owner.
    TooOpen {
        /// The path involved.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
    /// The path is a symbolic link, which a write will not replace.
    Symlink {
        /// The path involved.
        path: PathBuf,
    },
    /// The value to store is not usable.
    InvalidValue {
        /// What it was meant to be.
        what: &'static str,
        /// Why it was refused.
        reason: &'static str,
    },
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Malformed { path, line, column } => {
                write!(f, "{}: not valid TOML", path.display())?;
                if let (Some(line), Some(column)) = (line, column) {
                    write!(f, " at line {line}, column {column}")?;
                }
                Ok(())
            }
            Self::TooOpen { path, mode } => write!(
                f,
                "{} can be read by other users (mode {:03o}); run `chmod 600 {}` and try again",
                path.display(),
                mode & 0o777,
                path.display()
            ),
            Self::Symlink { path } => write!(
                f,
                "{} is a symbolic link; remove it so polaris can write a private file there",
                path.display()
            ),
            Self::InvalidValue { what, reason } => write!(f, "the {what} {reason}"),
        }
    }
}

impl std::error::Error for CredentialError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Where a run looks for credentials when its environment has none.
///
/// A trait, so the startup check can be run against a stand-in.
pub trait StoredCredentials {
    /// The stored key for a provider that takes one, if there is one.
    ///
    /// # Errors
    ///
    /// [`CredentialError`] when the store exists but cannot be used.
    fn api_key(&self, provider: Provider) -> Result<Option<String>, CredentialError>;

    /// The stored AWS profile name for Bedrock, if there is one.
    ///
    /// # Errors
    ///
    /// [`CredentialError`] when the store exists but cannot be used.
    fn aws_profile(&self) -> Result<Option<String>, CredentialError>;
}

/// Nothing is stored. What a run with no usable user directory sees.
impl StoredCredentials for Option<CredentialStore> {
    fn api_key(&self, provider: Provider) -> Result<Option<String>, CredentialError> {
        self.as_ref()
            .map_or(Ok(None), |store| store.api_key(provider))
    }

    fn aws_profile(&self) -> Result<Option<String>, CredentialError> {
        self.as_ref().map_or(Ok(None), CredentialStore::aws_profile)
    }
}

/// The credentials file, at one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialStore {
    path: PathBuf,
}

impl CredentialStore {
    /// A store at an explicit path.
    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The store in the user configuration directory, or `None` when no
    /// directory can be determined (no `POLARIS_CONFIG_DIR`, `XDG_CONFIG_HOME`
    /// or home directory).
    #[must_use]
    pub fn user() -> Option<Self> {
        config_dir::config_file(CREDENTIALS_FILE).map(Self::at)
    }

    /// The file this store reads and writes.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Save `key` for `provider`, replacing any earlier one.
    ///
    /// # Errors
    ///
    /// [`CredentialError`] when the key is not usable or the file cannot be
    /// written privately.
    pub fn set_api_key(&self, provider: Provider, key: &str) -> Result<(), CredentialError> {
        if provider.key_var().is_none() {
            return Err(CredentialError::InvalidValue {
                what: "provider",
                reason: "does not accept an API key; configure an AWS profile instead",
            });
        }
        let key = checked("API key", key)?;
        self.update(|document| set_field(document, provider.flag(), "api_key", &key))
    }

    /// Save the AWS profile Bedrock runs use when `AWS_PROFILE` is not set.
    ///
    /// # Errors
    ///
    /// [`CredentialError`] when the name is not usable or the file cannot be
    /// written privately.
    pub fn set_aws_profile(&self, profile: &str) -> Result<(), CredentialError> {
        let profile = checked("AWS profile name", profile)?;
        self.update(|document| {
            set_field(document, Provider::Bedrock.flag(), "profile", &profile);
        })
    }

    /// Forget what is stored for `provider`. `true` when there was something.
    ///
    /// The file is removed when nothing else is left in it.
    ///
    /// # Errors
    ///
    /// [`CredentialError`] when the file exists but cannot be used.
    pub fn remove(&self, provider: Provider) -> Result<bool, CredentialError> {
        let Some(mut document) = self.read()? else {
            return Ok(false);
        };
        if document.remove(provider.flag()).is_none() {
            return Ok(false);
        }
        if document.iter().next().is_none() {
            std::fs::remove_file(&self.path).map_err(|source| self.io(source))?;
        } else {
            self.write(&document)?;
        }
        Ok(true)
    }

    fn io(&self, source: std::io::Error) -> CredentialError {
        CredentialError::Io {
            path: self.path.clone(),
            source,
        }
    }

    /// The parsed file, or `None` when there is not one yet.
    fn read(&self) -> Result<Option<DocumentMut>, CredentialError> {
        let metadata = match std::fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(self.io(source)),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = metadata.permissions().mode();
            if mode & 0o077 != 0 {
                return Err(CredentialError::TooOpen {
                    path: self.path.clone(),
                    mode,
                });
            }
        }
        #[cfg(not(unix))]
        let _ = metadata;
        let text = std::fs::read_to_string(&self.path).map_err(|source| self.io(source))?;
        text.parse::<DocumentMut>().map(Some).map_err(|err| {
            // Parser diagnostics include the source line, which may hold a key.
            // Retain only its location; even Debug and source chains stay safe.
            let location = err.span().map(|span| {
                let before = &text[..span.start.min(text.len())];
                let line = before.bytes().filter(|byte| *byte == b'\n').count() + 1;
                let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
                (line, column)
            });
            CredentialError::Malformed {
                path: self.path.clone(),
                line: location.map(|(line, _)| line),
                column: location.map(|(_, column)| column),
            }
        })
    }

    fn stored(&self, provider: Provider, field: &str) -> Result<Option<String>, CredentialError> {
        Ok(self.read()?.and_then(|document| {
            document
                .get(provider.flag())
                .and_then(Item::as_table_like)
                .and_then(|table| table.get(field))
                .and_then(Item::as_str)
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty())
        }))
    }

    fn update(&self, change: impl FnOnce(&mut DocumentMut)) -> Result<(), CredentialError> {
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(CredentialError::Symlink {
                path: self.path.clone(),
            });
        }
        let mut document = self.read()?.unwrap_or_default();
        change(&mut document);
        self.write(&document)
    }

    /// Replace the file with `document`, atomically and readable by its owner
    /// only: a temporary file in the same directory is written, then renamed.
    fn write(&self, document: &DocumentMut) -> Result<(), CredentialError> {
        let parent = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        create_private_dir(parent).map_err(|source| self.io(source))?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".credentials-")
            .tempfile_in(parent)
            .map_err(|source| self.io(source))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            temporary
                .as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|source| self.io(source))?;
        }
        temporary
            .write_all(document.to_string().as_bytes())
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|source| self.io(source))?;
        temporary
            .persist(&self.path)
            .map_err(|err| self.io(err.error))?;
        Ok(())
    }
}

impl StoredCredentials for CredentialStore {
    fn api_key(&self, provider: Provider) -> Result<Option<String>, CredentialError> {
        self.stored(provider, "api_key")
    }

    fn aws_profile(&self) -> Result<Option<String>, CredentialError> {
        self.stored(Provider::Bedrock, "profile")
    }
}

/// `value` without surrounding whitespace, or the reason it cannot be stored:
/// empty, absurdly long, or holding whitespace or control characters, none of
/// which a real key or profile name has.
fn checked(what: &'static str, value: &str) -> Result<String, CredentialError> {
    let value = value.trim();
    let reason = if value.is_empty() {
        Some("is empty")
    } else if value.len() > MAX_VALUE_LEN {
        Some("is too long")
    } else if !value.chars().all(|c| c.is_ascii_graphic()) {
        Some("contains spaces, control characters or non-ASCII characters")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(CredentialError::InvalidValue { what, reason }),
        None => Ok(value.to_string()),
    }
}

fn set_field(document: &mut DocumentMut, table: &str, field: &str, text: &str) {
    let entry = document
        .as_table_mut()
        .entry(table)
        .or_insert_with(|| Item::Table(Table::new()));
    if let Some(table) = entry.as_table_like_mut() {
        table.insert(field, value(text));
    }
}

/// Create `dir` and any missing parents, readable by their owner only.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// A key as it may be shown: its last four characters, never more.
///
/// A key too short to show part of without giving much of it away is shown as
/// nothing but its length.
#[must_use]
pub fn mask_key(key: &str) -> String {
    let key = key.trim();
    let count = key.chars().count();
    if count < 12 {
        return format!("({count} characters)");
    }
    let tail: String = key.chars().skip(count - 4).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, CredentialStore) {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let store = CredentialStore::at(dir.path().join("polaris").join(CREDENTIALS_FILE));
        (dir, store)
    }

    #[test]
    fn a_saved_key_is_read_back_for_its_own_provider_only() {
        let (_dir, store) = store();
        store
            .set_api_key(Provider::Anthropic, "sk-ant-api03-abcdef")
            .unwrap();
        assert_eq!(
            store.api_key(Provider::Anthropic).unwrap().as_deref(),
            Some("sk-ant-api03-abcdef")
        );
        assert_eq!(store.api_key(Provider::OpenAi).unwrap(), None);
        assert_eq!(store.aws_profile().unwrap(), None);
    }

    #[test]
    fn saving_one_provider_keeps_the_others() {
        let (_dir, store) = store();
        store
            .set_api_key(Provider::Anthropic, "anthropic-key-1")
            .unwrap();
        store.set_api_key(Provider::OpenAi, "openai-key-1").unwrap();
        store.set_aws_profile("work").unwrap();
        store
            .set_api_key(Provider::Anthropic, "anthropic-key-2")
            .unwrap();

        assert_eq!(
            store.api_key(Provider::Anthropic).unwrap().as_deref(),
            Some("anthropic-key-2")
        );
        assert_eq!(
            store.api_key(Provider::OpenAi).unwrap().as_deref(),
            Some("openai-key-1")
        );
        assert_eq!(store.aws_profile().unwrap().as_deref(), Some("work"));
    }

    #[test]
    fn a_missing_file_holds_nothing() {
        let (_dir, store) = store();
        assert_eq!(store.api_key(Provider::Anthropic).unwrap(), None);
        assert!(!store.remove(Provider::Anthropic).unwrap());
        assert!(!store.path().exists(), "reading must not create the file");
    }

    #[cfg(unix)]
    #[test]
    fn the_file_and_its_directory_are_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_dir, store) = store();
        store.set_api_key(Provider::OpenAi, "openai-key-1").unwrap();

        let file = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(file & 0o777, 0o600, "the file is owner-only");
        let directory = std::fs::metadata(store.path().parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(directory & 0o777, 0o700, "the directory is owner-only");

        // An update keeps it that way.
        store.set_api_key(Provider::OpenAi, "openai-key-2").unwrap();
        let file = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(file & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn a_file_other_users_can_read_is_refused_and_the_fix_is_named() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_dir, store) = store();
        store.set_api_key(Provider::OpenAi, "openai-key-1").unwrap();
        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o644)).unwrap();

        let error = store.api_key(Provider::OpenAi).unwrap_err();
        assert!(matches!(error, CredentialError::TooOpen { .. }), "{error}");
        let message = error.to_string();
        assert!(message.contains("chmod 600"), "{message}");
        assert!(!message.contains("openai-key-1"), "{message}");
        assert!(
            store.set_api_key(Provider::OpenAi, "openai-key-2").is_err(),
            "a write must not quietly keep an exposed file"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_not_replaced() {
        let (dir, store) = store();
        let target = dir.path().join("elsewhere.toml");
        std::fs::write(&target, "").unwrap();
        std::fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, store.path()).unwrap();

        let error = store
            .set_api_key(Provider::OpenAi, "openai-key-1")
            .unwrap_err();
        assert!(matches!(error, CredentialError::Symlink { .. }), "{error}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "");
    }

    #[test]
    fn a_malformed_file_is_reported_without_echoing_its_contents() {
        let (_dir, store) = store();
        store.set_api_key(Provider::OpenAi, "openai-key-1").unwrap();
        let secret = "sk-ant-api03-realistic-secret-key-123456789";
        let malformed = format!("[anthropic]\napi_key = {secret}\n");
        std::fs::write(store.path(), &malformed).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let error = store.api_key(Provider::OpenAi).unwrap_err();
        assert!(
            matches!(error, CredentialError::Malformed { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("not valid TOML"), "{error}");
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(!rendered.contains(secret), "{rendered}");
        }
        assert!(error.to_string().contains("line 2, column 11"), "{error}");
        assert_eq!(
            std::fs::read_to_string(store.path()).unwrap(),
            malformed,
            "a file that cannot be read is left as it was"
        );
    }

    #[test]
    fn bedrock_does_not_accept_api_keys() {
        let (_dir, store) = store();
        assert!(matches!(
            store.set_api_key(Provider::Bedrock, "sk-unused"),
            Err(CredentialError::InvalidValue {
                what: "provider",
                ..
            })
        ));
        assert!(!store.path().exists());
    }

    #[test]
    fn unusable_values_are_refused_before_anything_is_written() {
        let (_dir, store) = store();
        for bad in [
            "",
            "   ",
            "two words",
            "line\nbreak",
            "tab\tbed",
            "ключ-key",
            "\u{7f}",
        ] {
            let error = store.set_api_key(Provider::Anthropic, bad).unwrap_err();
            assert!(
                matches!(error, CredentialError::InvalidValue { .. }),
                "{bad:?}: {error}"
            );
        }
        let too_long = "k".repeat(MAX_VALUE_LEN + 1);
        assert!(store.set_api_key(Provider::Anthropic, &too_long).is_err());
        assert!(store.set_aws_profile("has space").is_err());
        assert!(!store.path().exists());
    }

    #[test]
    fn a_key_is_saved_without_the_whitespace_a_paste_brings() {
        let (_dir, store) = store();
        store
            .set_api_key(Provider::Anthropic, "  sk-ant-key-1234\n")
            .unwrap();
        assert_eq!(
            store.api_key(Provider::Anthropic).unwrap().as_deref(),
            Some("sk-ant-key-1234")
        );
    }

    #[test]
    fn removing_a_provider_leaves_the_rest_and_the_last_removal_deletes_the_file() {
        let (_dir, store) = store();
        store
            .set_api_key(Provider::Anthropic, "anthropic-key-1")
            .unwrap();
        store.set_api_key(Provider::OpenAi, "openai-key-1").unwrap();

        assert!(store.remove(Provider::Anthropic).unwrap());
        assert_eq!(store.api_key(Provider::Anthropic).unwrap(), None);
        assert_eq!(
            store.api_key(Provider::OpenAi).unwrap().as_deref(),
            Some("openai-key-1")
        );
        assert!(!store.remove(Provider::Anthropic).unwrap(), "already gone");

        assert!(store.remove(Provider::OpenAi).unwrap());
        assert!(
            !store.path().exists(),
            "an empty credentials file is removed"
        );
    }

    #[test]
    fn an_absent_store_holds_nothing() {
        let nothing: Option<CredentialStore> = None;
        assert_eq!(nothing.api_key(Provider::Anthropic).unwrap(), None);
        assert_eq!(nothing.aws_profile().unwrap(), None);
    }

    #[test]
    fn a_key_is_masked_to_its_last_four_characters_at_most() {
        assert_eq!(mask_key("sk-ant-api03-abcdefgh1234"), "…1234");
        assert_eq!(mask_key("  sk-ant-api03-abcdefgh1234 \n"), "…1234");
        assert_eq!(mask_key("short"), "(5 characters)");
        assert_eq!(mask_key("elevenchars"), "(11 characters)");
        for key in ["sk-ant-api03-abcdefgh1234", "0123456789ab"] {
            let shown = mask_key(key);
            assert!(!shown.contains(&key[..key.len() - 4]), "{shown}");
        }
    }

    #[test]
    fn the_user_store_lives_in_the_configuration_directory() {
        // Resolution itself is `config_dir`'s; this pins the file name.
        if let Some(store) = CredentialStore::user() {
            assert_eq!(
                store.path().file_name().and_then(|name| name.to_str()),
                Some(CREDENTIALS_FILE)
            );
        }
    }
}
