//! Stable identities used across topology discovery and evidence records.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use polaris::system::plugin::Version;

/// Semantic version of a topology or topology contract.
///
/// Cosmos reuses Polaris's version value instead of defining another semantic
/// version representation.
pub type TopologyVersion = Version;

/// Error returned when a stable topology identifier is malformed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentifierError {
    kind: &'static str,
    value: Arc<str>,
    reason: &'static str,
}

impl IdentifierError {
    /// The kind of identifier that failed validation.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        self.kind
    }

    /// The rejected identifier value.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// A stable explanation of the violated rule.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        self.reason
    }
}

impl fmt::Display for IdentifierError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid {} {:?}: {}",
            self.kind, self.value, self.reason
        )
    }
}

impl std::error::Error for IdentifierError {}

fn stable_identifier(kind: &'static str, value: Arc<str>) -> Result<Arc<str>, IdentifierError> {
    let invalid = |reason| IdentifierError {
        kind,
        value: Arc::clone(&value),
        reason,
    };

    if value.is_empty() {
        return Err(invalid("must not be empty"));
    }
    if value.len() > 128 {
        return Err(invalid("must contain at most 128 bytes"));
    }

    let mut characters = value.chars();
    let first = characters.next().expect("non-empty identifier");
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(invalid(
            "must start with an ASCII lowercase letter or digit",
        ));
    }
    if !value.chars().all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '.' | '-' | '_' | '/' | ':')
    }) {
        return Err(invalid(
            "may contain only ASCII lowercase letters, digits, '.', '-', '_', '/', or ':'",
        ));
    }
    let last = value.chars().next_back().expect("non-empty identifier");
    if !last.is_ascii_lowercase() && !last.is_ascii_digit() {
        return Err(invalid("must end with an ASCII lowercase letter or digit"));
    }

    Ok(value)
}

macro_rules! stable_id {
    ($(#[$attribute:meta])* $name:ident, $kind:literal) => {
        $(#[$attribute])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(Arc<str>);

        impl $name {
            /// Creates a validated identifier.
            pub fn new(value: impl Into<Arc<str>>) -> Result<Self, IdentifierError> {
                stable_identifier($kind, value.into()).map(Self)
            }

            /// Returns the stable string representation.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = IdentifierError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }
    };
}

stable_id!(
    /// Stable identity of one reusable topology definition.
    TopologyId,
    "topology ID"
);

stable_id!(
    /// Stable identity of one semantic topology contract.
    TopologyContractId,
    "topology contract ID"
);

stable_id!(
    /// Stable identity of one concrete embodied catalog registration.
    RegistrationId,
    "registration ID"
);

/// A topology identity coupled to the version of its behaviour.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TopologyKey {
    id: TopologyId,
    version: TopologyVersion,
}

impl TopologyKey {
    /// Creates a versioned topology key.
    #[must_use]
    pub const fn new(id: TopologyId, version: TopologyVersion) -> Self {
        Self { id, version }
    }

    /// Returns the stable topology identity.
    #[must_use]
    pub fn id(&self) -> &TopologyId {
        &self.id
    }

    /// Returns the topology behaviour version.
    #[must_use]
    pub const fn version(&self) -> TopologyVersion {
        self.version
    }
}

impl fmt::Display for TopologyKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}@{}", self.id, self.version)
    }
}

/// A topology-contract identity coupled to its semantic version.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TopologyContractKey {
    id: TopologyContractId,
    version: TopologyVersion,
}

impl TopologyContractKey {
    /// Creates a versioned topology-contract key.
    #[must_use]
    pub const fn new(id: TopologyContractId, version: TopologyVersion) -> Self {
        Self { id, version }
    }

    /// Returns the stable contract identity.
    #[must_use]
    pub fn id(&self) -> &TopologyContractId {
        &self.id
    }

    /// Returns the contract version.
    #[must_use]
    pub const fn version(&self) -> TopologyVersion {
        self.version
    }
}

impl fmt::Display for TopologyContractKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}@{}", self.id, self.version)
    }
}

/// Stable digest of the configuration embodied by a registration.
///
/// The value should include its algorithm prefix, for example
/// `sha256:52f0abcd`. Cosmos treats the digest as opaque and only validates that
/// it is a bounded, printable token.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConfigurationDigest(Arc<str>);

impl ConfigurationDigest {
    /// Creates a validated configuration digest.
    pub fn new(value: impl Into<Arc<str>>) -> Result<Self, IdentifierError> {
        let value = value.into();
        let invalid = |reason| IdentifierError {
            kind: "configuration digest",
            value: Arc::clone(&value),
            reason,
        };

        if value.is_empty() {
            return Err(invalid("must not be empty"));
        }
        if value.len() > 256 {
            return Err(invalid("must contain at most 256 bytes"));
        }
        if !value.chars().all(|character| character.is_ascii_graphic()) {
            return Err(invalid("must contain only printable non-whitespace ASCII"));
        }

        Ok(Self(value))
    }

    /// Returns the opaque digest string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ConfigurationDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Provenance attached to a concrete topology registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationProvenance {
    provider: Arc<str>,
}

impl RegistrationProvenance {
    /// Creates provenance for a stable provider identifier.
    pub fn new(provider: impl Into<Arc<str>>) -> Result<Self, IdentifierError> {
        stable_identifier("registration provider", provider.into())
            .map(|provider| Self { provider })
    }

    /// Returns the plugin, pack, embodiment, or application that registered the
    /// topology.
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }
}
