//! Descriptive metadata for a registered composition block.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::identity::{FactKey, StablePortKey};
use crate::contracts::ContractError;

fn non_empty(field: &'static str, value: impl Into<Arc<str>>) -> Result<Arc<str>, ContractError> {
    let value = value.into();
    if value.trim().is_empty() {
        return Err(ContractError::new(
            "block-description.empty",
            format!("{field} must not be empty"),
        ));
    }
    Ok(value)
}

/// Discoverable metadata for one registered composition block: what it means,
/// what it reads and produces, and what a host must already know is true for
/// it to be worth considering.
///
/// Purely descriptive. It supplements [`BlockFactory`](super::BlockFactory),
/// [`ConfigurationSchemaKey`](crate::contracts::ConfigurationSchemaKey), and
/// [`Effect`](super::Effect) — already declared at registration — rather than
/// duplicating or overriding them, and it never changes what a plan naming
/// the block actually does: [`required_facts`](Self::required_facts) is read
/// only by [`admissible_blocks`](super::CompositionCatalogueSnapshot::admissible_blocks),
/// an opt-in filter a host may call before planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockDescription {
    summary: Arc<str>,
    inputs: Vec<StablePortKey>,
    output: StablePortKey,
    required_facts: BTreeSet<FactKey>,
    example: Arc<str>,
}

impl BlockDescription {
    /// Creates a description with no required facts; add them with
    /// [`requires`](Self::requires).
    ///
    /// `inputs` names the block's wired input ports in declaration order;
    /// `output` names the single type it produces. Both are the same
    /// [`StablePortKey`] identities a [`BlockFactory::wire_input`]
    /// implementation checks against, so a host reads a block's data shape
    /// from one type identity rather than a hand-written string.
    ///
    /// [`BlockFactory::wire_input`]: super::BlockFactory::wire_input
    pub fn new(
        summary: impl Into<Arc<str>>,
        inputs: Vec<StablePortKey>,
        output: StablePortKey,
        example: impl Into<Arc<str>>,
    ) -> Result<Self, ContractError> {
        Ok(Self {
            summary: non_empty("summary", summary)?,
            inputs,
            output,
            required_facts: BTreeSet::new(),
            example: non_empty("example", example)?,
        })
    }

    /// Declares one opaque host-defined fact this block requires to be
    /// admissible. Cosmos never interprets the fact's meaning. Repeating a
    /// fact is idempotent.
    #[must_use]
    pub fn requires(mut self, fact: FactKey) -> Self {
        self.required_facts.insert(fact);
        self
    }

    /// Returns the short summary of what the block does.
    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// Returns the block's wired input ports, in declaration order.
    #[must_use]
    pub fn inputs(&self) -> &[StablePortKey] {
        &self.inputs
    }

    /// Returns the type the block produces.
    #[must_use]
    pub fn output(&self) -> &StablePortKey {
        &self.output
    }

    /// Returns the facts a host must satisfy for this block to be
    /// admissible.
    #[must_use]
    pub fn required_facts(&self) -> &BTreeSet<FactKey> {
        &self.required_facts
    }

    /// Returns one example configured use of the block.
    #[must_use]
    pub fn example(&self) -> &str {
        &self.example
    }
}
