//! Stable semantic, host, event, schema, and content identities.

use std::fmt;
use std::str::FromStr;

use polaris::system::plugin::Version;
use serde::de::Error as _;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};

use super::ContractError;

fn validate_stable_name(kind: &'static str, value: &str) -> Result<(), ContractError> {
    if value.is_empty() {
        return Err(ContractError::new(
            "identity.empty",
            format!("{kind} must not be empty"),
        ));
    }
    if value.len() > 128 {
        return Err(ContractError::new(
            "identity.too-long",
            format!("{kind} must contain at most 128 bytes"),
        ));
    }
    let first = value.as_bytes()[0];
    let last = value.as_bytes()[value.len() - 1];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(ContractError::new(
            "identity.invalid-start",
            format!("{kind} must start with an ASCII lowercase letter or digit"),
        ));
    }
    if !(last.is_ascii_lowercase() || last.is_ascii_digit()) {
        return Err(ContractError::new(
            "identity.invalid-end",
            format!("{kind} must end with an ASCII lowercase letter or digit"),
        ));
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || matches!(byte, b'.' | b'-' | b'_' | b'/' | b':')
    }) {
        return Err(ContractError::new(
            "identity.invalid-character",
            format!(
                "{kind} may contain only lowercase ASCII letters, digits, '.', '-', '_', '/', or ':'"
            ),
        ));
    }
    Ok(())
}

macro_rules! semantic_name {
    ($(#[$attribute:meta])* $name:ident, $kind:literal) => {
        $(#[$attribute])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Creates a validated semantic name.
            pub fn new(value: impl Into<String>) -> Result<Self, ContractError> {
                let value = value.into();
                validate_stable_name($kind, &value)?;
                Ok(Self(value))
            }

            /// Returns the stable wire spelling.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = ContractError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(D::Error::custom)
            }
        }
    };
}

semantic_name!(
    /// Stable identity of a selector definition.
    SelectorId,
    "selector ID"
);
semantic_name!(
    /// Stable identity of a provider.
    ProviderId,
    "provider ID"
);
semantic_name!(
    /// Stable identity of a provenance source.
    SourceId,
    "source ID"
);
semantic_name!(
    /// Stable identity of a named policy.
    PolicyId,
    "policy ID"
);
semantic_name!(
    /// Stable identity of an application-owned subject schema.
    SubjectSchemaId,
    "subject schema ID"
);
semantic_name!(
    /// Stable identity of an evidence store.
    EvidenceStoreId,
    "evidence store ID"
);
semantic_name!(
    /// Stable object identity within an evidence store.
    EvidenceObjectId,
    "evidence object ID"
);
semantic_name!(
    /// Extensible identity of one comparable measurement.
    MeasurementId,
    "measurement ID"
);
semantic_name!(
    /// Extensible code explaining a mechanical or policy exclusion.
    ExclusionCode,
    "exclusion code"
);
semantic_name!(
    /// Extensible semantic capability identity.
    CapabilityId,
    "capability ID"
);
semantic_name!(
    /// Stable identity of a token accounting scheme.
    TokenAccountingId,
    "token accounting ID"
);
semantic_name!(
    /// Stable identity of a configuration schema.
    ConfigurationSchemaId,
    "configuration schema ID"
);
semantic_name!(
    /// Stable identity of a forbidden or observed data path.
    DataPathId,
    "data path ID"
);
semantic_name!(
    /// Stable identity of a tenancy scope.
    TenancyScopeId,
    "tenancy scope ID"
);
semantic_name!(
    /// Stable identity of one score component.
    ScoreComponentId,
    "score component ID"
);
semantic_name!(
    /// Stable subject key used inside extensible exclusion diagnostics.
    RecordSubjectId,
    "record subject ID"
);
semantic_name!(
    /// Stable model-family identity without provider-local secrets.
    ModelId,
    "model ID"
);
semantic_name!(
    /// Stable identity of an embodiment implementation.
    EmbodimentId,
    "embodiment ID"
);
semantic_name!(
    /// Stable identity of a host runtime implementation.
    HostRuntimeId,
    "host runtime ID"
);
semantic_name!(
    /// Stable semantic role-schedule identity used by mechanical run counts.
    RoleScheduleId,
    "role schedule ID"
);
semantic_name!(
    /// Stable class of a mechanical execution fault.
    ErrorClassId,
    "error class ID"
);
semantic_name!(
    /// Stable identity explaining one native context boundary type.
    ContextTypeId,
    "context type ID"
);
semantic_name!(
    /// Stable identity of an implemented runtime mechanism.
    RuntimeMechanismId,
    "runtime mechanism ID"
);
semantic_name!(
    /// Stable identity of an execution environment.
    EnvironmentId,
    "environment ID"
);
semantic_name!(
    /// Secret-free alias for a host-resolved binding.
    BindingAlias,
    "binding alias"
);
semantic_name!(
    /// Stable category of a host-resolved binding.
    BindingKindId,
    "binding kind ID"
);
semantic_name!(
    /// Extensible policy label reported by an environment.
    PolicyLabelId,
    "policy label ID"
);
semantic_name!(
    /// Stable identity of a sealed catalogue of approved composition recipes.
    CompositionCatalogId,
    "composition catalogue ID"
);
semantic_name!(
    /// Stable identity of one approved finite composition recipe.
    CompositionRecipeId,
    "composition recipe ID"
);
semantic_name!(
    /// Stable identity of one approved reusable composition block.
    CompositionBlockId,
    "composition block ID"
);

/// A semantic identity coupled to Polaris's semantic [`Version`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VersionedKey<I> {
    id: I,
    version: Version,
}

impl<I> VersionedKey<I> {
    /// Creates a versioned semantic key.
    #[must_use]
    pub const fn new(id: I, version: Version) -> Self {
        Self { id, version }
    }

    /// Returns the semantic identity.
    #[must_use]
    pub const fn id(&self) -> &I {
        &self.id
    }

    /// Returns the semantic version.
    #[must_use]
    pub const fn version(&self) -> Version {
        self.version
    }
}

impl<I: fmt::Display> fmt::Display for VersionedKey<I> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}@{}", self.id, self.version)
    }
}

impl<I: Serialize> Serialize for VersionedKey<I> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("VersionedKey", 2)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("version", &VersionWire::from(self.version))?;
        state.end()
    }
}

impl<'de, I: Deserialize<'de>> Deserialize<'de> for VersionedKey<I> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = VersionedKeyWire::<I>::deserialize(deserializer)?;
        Ok(Self::new(wire.id, wire.version.into()))
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct VersionWire {
    major: u64,
    minor: u64,
    patch: u64,
}

impl From<Version> for VersionWire {
    fn from(version: Version) -> Self {
        Self {
            major: version.major,
            minor: version.minor,
            patch: version.patch,
        }
    }
}

impl From<VersionWire> for Version {
    fn from(version: VersionWire) -> Self {
        Self::new(version.major, version.minor, version.patch)
    }
}

#[derive(Deserialize)]
struct VersionedKeyWire<I> {
    id: I,
    version: VersionWire,
}

/// Stable versioned selector identity.
pub type SelectorKey = VersionedKey<SelectorId>;
/// Stable versioned provider identity.
pub type ProviderKey = VersionedKey<ProviderId>;
/// Stable versioned provenance-source identity.
pub type SourceKey = VersionedKey<SourceId>;
/// Stable versioned policy identity.
pub type PolicyRef = VersionedKey<PolicyId>;
/// Stable versioned subject-schema identity.
pub type SubjectSchemaKey = VersionedKey<SubjectSchemaId>;
/// Stable versioned configuration-schema identity.
pub type ConfigurationSchemaKey = VersionedKey<ConfigurationSchemaId>;
/// Stable versioned embodiment identity.
pub type EmbodimentKey = VersionedKey<EmbodimentId>;
/// Stable versioned host-runtime identity.
pub type HostRuntimeKey = VersionedKey<HostRuntimeId>;
/// Stable versioned semantic capability identity.
pub type CapabilityKey = VersionedKey<CapabilityId>;
/// Stable versioned explanation of one native context boundary type.
pub type ContextTypeKey = VersionedKey<ContextTypeId>;
/// Stable versioned runtime-mechanism identity.
pub type RuntimeMechanismKey = VersionedKey<RuntimeMechanismId>;
/// Stable versioned execution-environment identity.
pub type EnvironmentKey = VersionedKey<EnvironmentId>;
/// Stable versioned identity of a sealed composition catalogue.
pub type CompositionCatalogKey = VersionedKey<CompositionCatalogId>;
/// Stable versioned identity of one approved composition recipe.
pub type CompositionRecipeKey = VersionedKey<CompositionRecipeId>;
/// Stable versioned identity of one approved composition block.
pub type CompositionBlockKey = VersionedKey<CompositionBlockId>;

macro_rules! host_reference {
    ($(#[$attribute:meta])* $name:ident, $kind:literal) => {
        $(#[$attribute])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        pub struct $name {
            namespace: String,
            id: String,
        }

        impl $name {
            /// Creates a validated namespaced host reference.
            pub fn new(
                namespace: impl Into<String>,
                id: impl Into<String>,
            ) -> Result<Self, ContractError> {
                let namespace = namespace.into();
                let id = id.into();
                validate_stable_name(concat!($kind, " namespace"), &namespace)?;
                validate_stable_name(concat!($kind, " value"), &id)?;
                Ok(Self { namespace, id })
            }

            /// Returns the namespace assigned by the host.
            #[must_use]
            pub fn namespace(&self) -> &str {
                &self.namespace
            }

            /// Returns the opaque value within the namespace.
            #[must_use]
            pub fn id(&self) -> &str {
                &self.id
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                #[derive(Deserialize)]
                struct Wire {
                    namespace: String,
                    id: String,
                }
                let wire = Wire::deserialize(deserializer)?;
                Self::new(wire.namespace, wire.id).map_err(D::Error::custom)
            }
        }
    };
}

host_reference!(
    /// Host-owned identity of one task without embedding its payload.
    TaskId,
    "task ID"
);
host_reference!(
    /// Optional host-owned reference to an external session.
    ExternalSessionRef,
    "external session reference"
);

macro_rules! event_identity {
    ($(#[$attribute:meta])* $name:ident, $kind:literal) => {
        $(#[$attribute])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; 16]);

        impl $name {
            /// Creates a typed `UUIDv4` event identity from validated bytes.
            pub fn from_bytes(bytes: [u8; 16]) -> Result<Self, ContractError> {
                validate_uuid_v4($kind, &bytes)?;
                Ok(Self(bytes))
            }

            /// Returns the UUID bytes.
            #[must_use]
            pub const fn as_bytes(&self) -> &[u8; 16] {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = ContractError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                parse_uuid_v4($kind, value).and_then(Self::from_bytes)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                format_uuid(self.0, formatter)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                String::deserialize(deserializer)?
                    .parse()
                    .map_err(D::Error::custom)
            }
        }
    };
}

event_identity!(
    /// Identity of one selection decision event.
    SelectionDecisionId,
    "selection decision ID"
);
event_identity!(
    /// Identity of one live admission attempt event.
    AdmissionAttemptId,
    "admission attempt ID"
);
event_identity!(
    /// Identity of one started topology execution.
    RunId,
    "run ID"
);
event_identity!(
    /// Identity of one outcome observation event.
    OutcomeObservationId,
    "outcome observation ID"
);
event_identity!(
    /// Identity of one runtime-availability observation event.
    AvailabilitySnapshotId,
    "availability snapshot ID"
);

fn validate_uuid_v4(kind: &'static str, bytes: &[u8; 16]) -> Result<(), ContractError> {
    if bytes[6] >> 4 != 4 || bytes[8] >> 6 != 2 {
        return Err(ContractError::new(
            "identity.not-uuid-v4",
            format!("{kind} must be a UUIDv4 with an RFC 4122 variant"),
        ));
    }
    Ok(())
}

fn parse_uuid_v4(kind: &'static str, value: &str) -> Result<[u8; 16], ContractError> {
    if value.len() != 36
        || value.as_bytes()[8] != b'-'
        || value.as_bytes()[13] != b'-'
        || value.as_bytes()[18] != b'-'
        || value.as_bytes()[23] != b'-'
    {
        return Err(ContractError::new(
            "identity.uuid-format",
            format!("{kind} must use lowercase hyphenated UUID form"),
        ));
    }
    let mut bytes = [0_u8; 16];
    let mut source = value.bytes().filter(|byte| *byte != b'-');
    for byte in &mut bytes {
        let high = lowercase_hex(source.next(), kind)?;
        let low = lowercase_hex(source.next(), kind)?;
        *byte = (high << 4) | low;
    }
    validate_uuid_v4(kind, &bytes)?;
    Ok(bytes)
}

fn lowercase_hex(byte: Option<u8>, kind: &'static str) -> Result<u8, ContractError> {
    match byte {
        Some(byte @ b'0'..=b'9') => Ok(byte - b'0'),
        Some(byte @ b'a'..=b'f') => Ok(byte - b'a' + 10),
        _ => Err(ContractError::new(
            "identity.uuid-format",
            format!("{kind} must use lowercase hexadecimal digits"),
        )),
    }
}

fn format_uuid(bytes: [u8; 16], formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            formatter.write_str("-")?;
        }
        write!(formatter, "{byte:02x}")?;
    }
    Ok(())
}

/// A SHA-256 content identity rendered with its algorithm prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentDigest([u8; 32]);

impl ContentDigest {
    /// Hashes canonical bytes into a content identity.
    #[must_use]
    pub fn sha256(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// Creates an identity from an already-computed SHA-256 value.
    #[must_use]
    pub const fn from_sha256(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the raw SHA-256 value.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl FromStr for ContentDigest {
    type Err = ContractError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let hex = value.strip_prefix("sha256:").ok_or_else(|| {
            ContractError::new(
                "identity.digest-algorithm",
                "content digest must start with 'sha256:'",
            )
        })?;
        if hex.len() != 64 {
            return Err(ContractError::new(
                "identity.digest-length",
                "SHA-256 content digest must contain 64 lowercase hexadecimal digits",
            ));
        }
        let mut bytes = [0_u8; 32];
        let mut source = hex.bytes();
        for byte in &mut bytes {
            let high = lowercase_hex(source.next(), "content digest")?;
            let low = lowercase_hex(source.next(), "content digest")?;
            *byte = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for ContentDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("sha256:")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl Serialize for ContentDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ContentDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

/// Unsigned major version of one persisted record schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct SchemaVersion(u64);

impl SchemaVersion {
    /// The first shared contract schema major.
    pub const V1: Self = Self(1);

    /// Creates a non-zero schema major.
    pub fn new(major: u64) -> Result<Self, ContractError> {
        if major == 0 {
            Err(ContractError::new(
                "schema.zero",
                "schema major versions start at one",
            ))
        } else {
            Ok(Self(major))
        }
    }

    /// Returns the unsigned major version.
    #[must_use]
    pub const fn major(self) -> u64 {
        self.0
    }

    pub(crate) fn require_v1(self) -> Result<(), ContractError> {
        if self == Self::V1 {
            Ok(())
        } else {
            Err(ContractError::new(
                "schema.unsupported-major",
                format!("expected schema major 1, found {}", self.0),
            ))
        }
    }
}

impl<'de> Deserialize<'de> for SchemaVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u64::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}
