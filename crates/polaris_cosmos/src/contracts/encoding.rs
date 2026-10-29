//! Deterministic CBOR and content-identity support.

use std::fmt;

use ciborium::value::Value;
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::ContentDigest;

/// Validation or canonical wire error returned by shared contracts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractError {
    code: &'static str,
    detail: String,
}

impl ContractError {
    /// Creates a stable coded contract error.
    #[must_use]
    pub(crate) fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    /// Returns the stable machine-readable error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    /// Returns the human-readable error detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for ContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for ContractError {}

/// A validated record with a deterministic CBOR wire form.
///
/// Decoding rejects non-canonical CBOR before record-specific validation. A
/// record implementation must also verify its schema major, invariants, and
/// embedded content digest in [`validate`](Self::validate).
pub trait CanonicalRecord: Serialize + DeserializeOwned + Sized {
    /// Validates the decoded record and its content identity.
    fn validate(&self) -> Result<(), ContractError>;

    /// Encodes the complete record as deterministic CBOR.
    fn to_canonical_cbor(&self) -> Result<Vec<u8>, ContractError> {
        self.validate()?;
        canonical_cbor(self)
    }

    /// Decodes one deterministic CBOR record.
    fn from_canonical_cbor(bytes: &[u8]) -> Result<Self, ContractError> {
        require_canonical(bytes)?;
        let value: Self = ciborium::de::from_reader(bytes).map_err(|error| {
            ContractError::new("wire.decode", format!("invalid CBOR record: {error}"))
        })?;
        value.validate()?;
        Ok(value)
    }
}

pub(crate) fn canonical_cbor(value: &impl Serialize) -> Result<Vec<u8>, ContractError> {
    let value = Value::serialized(value).map_err(|error| {
        ContractError::new(
            "wire.serialize",
            format!("value cannot be represented as CBOR: {error}"),
        )
    })?;
    let mut encoded = Vec::new();
    encode_value(&value, &mut encoded)?;
    Ok(encoded)
}

pub(crate) fn content_digest(value: &impl Serialize) -> Result<ContentDigest, ContractError> {
    canonical_cbor(value).map(|bytes| ContentDigest::sha256(&bytes))
}

fn require_canonical(bytes: &[u8]) -> Result<(), ContractError> {
    let value: Value = ciborium::de::from_reader(bytes).map_err(|error| {
        ContractError::new("wire.decode", format!("invalid CBOR value: {error}"))
    })?;
    let mut encoded = Vec::new();
    encode_value(&value, &mut encoded)?;
    if encoded == bytes {
        Ok(())
    } else {
        Err(ContractError::new(
            "wire.non-canonical",
            "record is valid CBOR but not the deterministic contract encoding",
        ))
    }
}

fn encode_value(value: &Value, output: &mut Vec<u8>) -> Result<(), ContractError> {
    match value {
        Value::Integer(integer) => {
            let integer = i128::from(*integer);
            if integer >= 0 {
                let integer = u64::try_from(integer).map_err(|_| {
                    ContractError::new("wire.integer-range", "unsigned integer exceeds u64")
                })?;
                encode_head(0, integer, output);
            } else {
                let magnitude = u64::try_from(-1 - integer).map_err(|_| {
                    ContractError::new("wire.integer-range", "negative integer exceeds i64 range")
                })?;
                encode_head(1, magnitude, output);
            }
        }
        Value::Bytes(bytes) => {
            encode_head(2, usize_as_u64(bytes.len())?, output);
            output.extend_from_slice(bytes);
        }
        Value::Text(text) => {
            encode_head(3, usize_as_u64(text.len())?, output);
            output.extend_from_slice(text.as_bytes());
        }
        Value::Array(values) => {
            encode_head(4, usize_as_u64(values.len())?, output);
            for value in values {
                encode_value(value, output)?;
            }
        }
        Value::Map(entries) => {
            let mut encoded_entries = Vec::with_capacity(entries.len());
            for (key, value) in entries {
                let mut encoded_key = Vec::new();
                encode_value(key, &mut encoded_key)?;
                let mut encoded_value = Vec::new();
                encode_value(value, &mut encoded_value)?;
                encoded_entries.push((encoded_key, encoded_value));
            }
            encoded_entries.sort_by(|left, right| {
                left.0
                    .len()
                    .cmp(&right.0.len())
                    .then_with(|| left.0.cmp(&right.0))
            });
            if encoded_entries
                .windows(2)
                .any(|pair| pair[0].0 == pair[1].0)
            {
                return Err(ContractError::new(
                    "wire.duplicate-map-key",
                    "canonical maps cannot contain duplicate encoded keys",
                ));
            }
            encode_head(5, usize_as_u64(encoded_entries.len())?, output);
            for (key, value) in encoded_entries {
                output.extend_from_slice(&key);
                output.extend_from_slice(&value);
            }
        }
        Value::Bool(boolean) => output.push(if *boolean { 0xf5 } else { 0xf4 }),
        Value::Null => output.push(0xf6),
        Value::Float(_) => {
            return Err(ContractError::new(
                "wire.float-forbidden",
                "shared contract values must use validated integer quantities",
            ));
        }
        Value::Tag(_, _) => {
            return Err(ContractError::new(
                "wire.tag-forbidden",
                "shared contract values do not use CBOR tags",
            ));
        }
        _ => {
            return Err(ContractError::new(
                "wire.unsupported-value",
                "unsupported CBOR value in shared contract",
            ));
        }
    }
    Ok(())
}

fn usize_as_u64(value: usize) -> Result<u64, ContractError> {
    u64::try_from(value)
        .map_err(|_| ContractError::new("wire.length-range", "collection length exceeds u64"))
}

fn encode_head(major: u8, value: u64, output: &mut Vec<u8>) {
    let prefix = major << 5;
    match value {
        0..=23 => output.push(prefix | value as u8),
        24..=0xff => output.extend_from_slice(&[prefix | 24, value as u8]),
        0x100..=0xffff => {
            output.push(prefix | 25);
            output.extend_from_slice(&(value as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            output.push(prefix | 26);
            output.extend_from_slice(&(value as u32).to_be_bytes());
        }
        _ => {
            output.push(prefix | 27);
            output.extend_from_slice(&value.to_be_bytes());
        }
    }
}
