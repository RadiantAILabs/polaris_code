//! Semantic topology and contract descriptions.

use std::fmt;
use std::sync::Arc;

use polaris::graph::GraphSignature;

use super::{TopologyContractKey, TopologyKey};

/// Error returned when required semantic metadata is absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataError {
    field: &'static str,
}

impl MetadataError {
    /// The empty metadata field.
    #[must_use]
    pub fn field(&self) -> &'static str {
        self.field
    }
}

impl fmt::Display for MetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "topology metadata field {:?} must not be empty",
            self.field
        )
    }
}

impl std::error::Error for MetadataError {}

fn non_empty(field: &'static str, value: impl Into<Arc<str>>) -> Result<Arc<str>, MetadataError> {
    let value = value.into();
    if value.trim().is_empty() {
        Err(MetadataError { field })
    } else {
        Ok(value)
    }
}

/// A stable semantic contract backed by a native Polaris graph signature.
///
/// Contracts name what a topology means while [`GraphSignature`] remains the
/// structural authority. A candidate is admitted when its derived signature
/// [`satisfies`](GraphSignature::satisfies) this contract and its descriptor
/// explicitly claims the same key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopologyContract {
    key: TopologyContractKey,
    signature: GraphSignature,
    task_semantics: Arc<str>,
    result_semantics: Arc<str>,
    outcome_interpretation: Option<Arc<str>>,
    documentation: Option<Arc<str>>,
}

impl TopologyContract {
    /// Creates a contract with required task and result semantics.
    pub fn new(
        key: TopologyContractKey,
        signature: GraphSignature,
        task_semantics: impl Into<Arc<str>>,
        result_semantics: impl Into<Arc<str>>,
    ) -> Result<Self, MetadataError> {
        Ok(Self {
            key,
            signature,
            task_semantics: non_empty("task_semantics", task_semantics)?,
            result_semantics: non_empty("result_semantics", result_semantics)?,
            outcome_interpretation: None,
            documentation: None,
        })
    }

    /// Adds the meaning of a successful or otherwise measured outcome.
    pub fn with_outcome_interpretation(
        mut self,
        interpretation: impl Into<Arc<str>>,
    ) -> Result<Self, MetadataError> {
        self.outcome_interpretation = Some(non_empty("outcome_interpretation", interpretation)?);
        Ok(self)
    }

    /// Adds a stable documentation reference for the semantic contract.
    pub fn with_documentation(
        mut self,
        documentation: impl Into<Arc<str>>,
    ) -> Result<Self, MetadataError> {
        self.documentation = Some(non_empty("documentation", documentation)?);
        Ok(self)
    }

    /// Returns the stable, versioned contract key.
    #[must_use]
    pub fn key(&self) -> &TopologyContractKey {
        &self.key
    }

    /// Returns the native structural contract.
    #[must_use]
    pub fn signature(&self) -> &GraphSignature {
        &self.signature
    }

    /// Returns the semantic meaning of the contract's task input.
    #[must_use]
    pub fn task_semantics(&self) -> &str {
        &self.task_semantics
    }

    /// Returns the semantic meaning of the contract's result.
    #[must_use]
    pub fn result_semantics(&self) -> &str {
        &self.result_semantics
    }

    /// Returns the documented outcome interpretation, when supplied.
    #[must_use]
    pub fn outcome_interpretation(&self) -> Option<&str> {
        self.outcome_interpretation.as_deref()
    }

    /// Returns the contract documentation reference, when supplied.
    #[must_use]
    pub fn documentation(&self) -> Option<&str> {
        self.documentation.as_deref()
    }
}

/// Immutable semantic metadata for one versioned topology definition.
///
/// The descriptor supplements a derived graph signature with discoverable
/// meaning. It never declares or overrides structural resource and output
/// requirements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopologyDescriptor {
    key: TopologyKey,
    title: Arc<str>,
    summary: Arc<str>,
    contracts: Vec<TopologyContractKey>,
}

impl TopologyDescriptor {
    /// Creates a descriptor with no contract claims.
    ///
    /// Call [`claim`](Self::claim) at least once before registration.
    pub fn new(
        key: TopologyKey,
        title: impl Into<Arc<str>>,
        summary: impl Into<Arc<str>>,
    ) -> Result<Self, MetadataError> {
        Ok(Self {
            key,
            title: non_empty("title", title)?,
            summary: non_empty("summary", summary)?,
            contracts: Vec::new(),
        })
    }

    /// Claims a semantic contract for this topology.
    ///
    /// Repeating a claim is idempotent.
    #[must_use]
    pub fn claim(mut self, contract: TopologyContractKey) -> Self {
        if !self.contracts.contains(&contract) {
            self.contracts.push(contract);
            self.contracts.sort();
        }
        self
    }

    /// Returns the stable, versioned topology key.
    #[must_use]
    pub fn key(&self) -> &TopologyKey {
        &self.key
    }

    /// Returns the human-readable topology title.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Returns the short semantic description.
    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// Returns the sorted semantic contracts this topology claims.
    #[must_use]
    pub fn contracts(&self) -> &[TopologyContractKey] {
        &self.contracts
    }
}
