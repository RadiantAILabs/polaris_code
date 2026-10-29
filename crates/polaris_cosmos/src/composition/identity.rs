//! Stable identities for gate-plan trees and the composition catalogue.

use std::fmt;
use std::str::FromStr;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::contracts::{ContractError, VersionedKey};

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

macro_rules! stable_name {
    ($(#[$attribute:meta])* $name:ident, $kind:literal) => {
        $(#[$attribute])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Creates a validated stable name.
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

stable_name!(
    /// Stable identity of a registered branch predicate.
    PredicateId,
    "predicate ID"
);
stable_name!(
    /// Stable identity of a registered `Switch` discriminator.
    DiscriminatorId,
    "discriminator ID"
);
stable_name!(
    /// Stable identity of a registered error-handler recipe.
    HandlerId,
    "handler ID"
);
stable_name!(
    /// Stable identity of a registered timeout policy.
    TimeoutPolicyId,
    "timeout policy ID"
);
stable_name!(
    /// Stable identity of a registered retry policy.
    RetryPolicyId,
    "retry policy ID"
);
stable_name!(
    /// Stable identity of a registered loop retention policy.
    RetentionPolicyId,
    "retention policy ID"
);
stable_name!(
    /// Stable identity of a registered scope context policy.
    ContextPolicyId,
    "context policy ID"
);
stable_name!(
    /// Stable identity of a registered typed output port.
    StablePortId,
    "stable port ID"
);

/// Stable versioned identity of a registered branch predicate.
pub type PredicateKey = VersionedKey<PredicateId>;
/// Stable versioned identity of a registered `Switch` discriminator.
pub type DiscriminatorKey = VersionedKey<DiscriminatorId>;
/// Stable versioned identity of a registered error-handler recipe.
pub type HandlerKey = VersionedKey<HandlerId>;
/// Stable versioned identity of a registered timeout policy.
pub type TimeoutPolicyKey = VersionedKey<TimeoutPolicyId>;
/// Stable versioned identity of a registered retry policy.
pub type RetryPolicyKey = VersionedKey<RetryPolicyId>;
/// Stable versioned identity of a registered loop retention policy.
pub type RetentionPolicyKey = VersionedKey<RetentionPolicyId>;
/// Stable versioned identity of a registered scope context policy.
pub type ContextPolicyKey = VersionedKey<ContextPolicyId>;
/// Stable versioned identity of a registered typed output port.
pub type StablePortKey = VersionedKey<StablePortId>;

stable_name!(
    /// Plan-local label for one block instance in a gate-expression tree.
    ///
    /// Distinct from a native Polaris `NodeId`: the host assigns node IDs
    /// during materialization, never the planner.
    BlockInstanceId,
    "block instance ID"
);
stable_name!(
    /// Key into a [`GatePlan`](super::GatePlan)'s configuration table.
    ConfigId,
    "config ID"
);
stable_name!(
    /// Label for one case in a `Switch` gate.
    CaseKey,
    "switch case key"
);
stable_name!(
    /// Opaque host-defined key naming one fact a host can assert is true or
    /// false about the run it is planning for (a resource being available, a
    /// task's own declared shape, and so on). Cosmos never interprets a
    /// fact's meaning: it only matches the keys a [`BlockDescription`]
    /// requires against the keys a host-supplied predicate satisfies.
    ///
    /// [`BlockDescription`]: super::BlockDescription
    FactKey,
    "fact key"
);
