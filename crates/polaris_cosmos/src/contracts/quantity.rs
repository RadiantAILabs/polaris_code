//! Validated integer quantities used on shared wire boundaries.

use std::fmt;
use std::str::FromStr;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

use super::{ContractError, TokenAccountingId};

macro_rules! ppm_quantity {
    ($(#[$attribute:meta])* $name:ident, $kind:literal) => {
        $(#[$attribute])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(u32);

        impl $name {
            /// One million parts per million.
            pub const ONE: Self = Self(1_000_000);
            /// Zero parts per million.
            pub const ZERO: Self = Self(0);

            /// Creates a validated parts-per-million quantity.
            pub fn new(value: u32) -> Result<Self, ContractError> {
                if value <= 1_000_000 {
                    Ok(Self(value))
                } else {
                    Err(ContractError::new(
                        "quantity.ppm-range",
                        format!("{} must be in 0..=1_000_000, found {value}", $kind),
                    ))
                }
            }

            /// Returns the integer parts-per-million value.
            #[must_use]
            pub const fn get(self) -> u32 {
                self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::new(u32::deserialize(deserializer)?).map_err(D::Error::custom)
            }
        }
    };
}

ppm_quantity!(
    /// Confidence in a known value or attributed judgement.
    ConfidencePpm,
    "confidence"
);
ppm_quantity!(
    /// Probability assigned to one outcome or selection candidate.
    ProbabilityPpm,
    "probability"
);
ppm_quantity!(
    /// Fractional coverage expressed without floating point.
    CoveragePpm,
    "coverage"
);
ppm_quantity!(
    /// Normalized integer utility used by explicit scoring policy.
    UtilityPpm,
    "utility"
);

/// One preference weight in integer basis points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct PreferenceBasisPoints(u16);

impl PreferenceBasisPoints {
    /// The complete 10,000-basis-point weight.
    pub const TOTAL: Self = Self(10_000);

    /// Creates a weight in `0..=10_000`.
    pub fn new(value: u16) -> Result<Self, ContractError> {
        if value <= 10_000 {
            Ok(Self(value))
        } else {
            Err(ContractError::new(
                "quantity.basis-points-range",
                format!("preference weight must be in 0..=10_000, found {value}"),
            ))
        }
    }

    /// Returns the integer basis-point value.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }

    /// Validates that a complete preference vector totals exactly 10,000.
    pub fn validate_total(weights: impl IntoIterator<Item = Self>) -> Result<(), ContractError> {
        let total = weights.into_iter().try_fold(0_u32, |total, weight| {
            total.checked_add(u32::from(weight.0)).ok_or_else(|| {
                ContractError::new(
                    "quantity.basis-points-overflow",
                    "preference weight total overflowed u32",
                )
            })
        })?;
        if total == 10_000 {
            Ok(())
        } else {
            Err(ContractError::new(
                "quantity.basis-points-total",
                format!("preference weights must total exactly 10_000, found {total}"),
            ))
        }
    }
}

impl<'de> Deserialize<'de> for PreferenceBasisPoints {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u16::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// Unsigned duration in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DurationMs(u64);

impl DurationMs {
    /// Creates a millisecond duration.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the number of milliseconds.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Inclusive unsigned cardinality estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CardinalityEstimate {
    min: u64,
    likely: u64,
    max: u64,
}

impl CardinalityEstimate {
    /// Creates an ordered `min <= likely <= max` estimate.
    pub fn new(min: u64, likely: u64, max: u64) -> Result<Self, ContractError> {
        if min <= likely && likely <= max {
            Ok(Self { min, likely, max })
        } else {
            Err(ContractError::new(
                "quantity.cardinality-order",
                format!("expected min <= likely <= max, found {min} <= {likely} <= {max}"),
            ))
        }
    }

    /// Returns the conservative lower bound.
    #[must_use]
    pub const fn min(self) -> u64 {
        self.min
    }

    /// Returns the most likely cardinality.
    #[must_use]
    pub const fn likely(self) -> u64 {
        self.likely
    }

    /// Returns the conservative upper bound.
    #[must_use]
    pub const fn max(self) -> u64 {
        self.max
    }
}

impl<'de> Deserialize<'de> for CardinalityEstimate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            min: u64,
            likely: u64,
            max: u64,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.min, wire.likely, wire.max).map_err(D::Error::custom)
    }
}

/// Counted tokens under one stable accounting scheme.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TokenQuantity {
    accounting_id: TokenAccountingId,
    count: u64,
}

impl TokenQuantity {
    /// Creates a token quantity with its comparison witness.
    #[must_use]
    pub const fn new(accounting_id: TokenAccountingId, count: u64) -> Self {
        Self {
            accounting_id,
            count,
        }
    }

    /// Returns the token accounting scheme.
    #[must_use]
    pub const fn accounting_id(&self) -> &TokenAccountingId {
        &self.accounting_id
    }

    /// Returns the counted tokens.
    #[must_use]
    pub const fn count(&self) -> u64 {
        self.count
    }

    /// Adds a comparable quantity, rejecting unlike accounting schemes.
    pub fn checked_add(&self, other: &Self) -> Result<Self, ContractError> {
        if self.accounting_id != other.accounting_id {
            return Err(ContractError::new(
                "quantity.incomparable-token-schemes",
                "token quantities require the same accounting ID or an explicit conversion policy",
            ));
        }
        let count = self.count.checked_add(other.count).ok_or_else(|| {
            ContractError::new(
                "quantity.overflow",
                "token quantity addition overflowed u64",
            )
        })?;
        Ok(Self::new(self.accounting_id.clone(), count))
    }
}

/// Uppercase ISO-4217 currency code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CurrencyCode([u8; 3]);

impl CurrencyCode {
    /// Returns the three-letter currency code.
    #[must_use]
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("currency code is validated ASCII")
    }
}

impl FromStr for CurrencyCode {
    type Err = ContractError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 3 || !value.bytes().all(|byte| byte.is_ascii_uppercase()) {
            return Err(ContractError::new(
                "quantity.currency-code",
                "currency must be an uppercase three-letter ISO-4217 code",
            ));
        }
        let mut bytes = [0_u8; 3];
        bytes.copy_from_slice(value.as_bytes());
        Ok(Self(bytes))
    }
}

impl fmt::Display for CurrencyCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for CurrencyCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CurrencyCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

/// Unsigned monetary quantity in micros with an explicit currency witness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Money {
    micros: u64,
    currency: CurrencyCode,
}

impl Money {
    /// Creates a monetary quantity.
    #[must_use]
    pub const fn new(micros: u64, currency: CurrencyCode) -> Self {
        Self { micros, currency }
    }

    /// Returns the unsigned micros.
    #[must_use]
    pub const fn micros(self) -> u64 {
        self.micros
    }

    /// Returns the currency witness.
    #[must_use]
    pub const fn currency(self) -> CurrencyCode {
        self.currency
    }

    /// Adds a comparable monetary quantity, rejecting unlike currencies.
    pub fn checked_add(self, other: Self) -> Result<Self, ContractError> {
        if self.currency != other.currency {
            return Err(ContractError::new(
                "quantity.incomparable-currencies",
                "money requires the same currency or an explicit conversion policy",
            ));
        }
        let micros = self.micros.checked_add(other.micros).ok_or_else(|| {
            ContractError::new("quantity.overflow", "money addition overflowed u64")
        })?;
        Ok(Self::new(micros, self.currency))
    }
}
