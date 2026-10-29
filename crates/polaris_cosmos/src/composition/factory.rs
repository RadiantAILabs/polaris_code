//! How an embodiment registers a block that adds node(s) to a graph under
//! construction.
//!
//! A [`BlockFactory`] is the only place embodiment-specific Rust types
//! appear in composition: it knows its own concrete system and port types,
//! and uses that knowledge to add nodes to the graph and to read a named
//! upstream output into its own input type. The gate-plan tree around it
//! never sees those types — only the [`StablePortKey`]s and
//! [`CompositionBlockKey`]s the embodiment registered them under.

use std::fmt;

use polaris::graph::{Graph, NodeId};

use super::identity::StablePortKey;
use crate::contracts::ResolvedConfigurationRef;

/// A registered block failed to add its node(s) to the graph being built.
#[derive(Debug, Clone)]
pub struct BlockBuildError {
    detail: String,
}

impl BlockBuildError {
    /// Creates a block-build failure with a human-readable detail.
    #[must_use]
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }

    /// Returns the human-readable failure detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for BlockBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for BlockBuildError {}

/// A plan-level `Wire` could not be realized between two block instances.
#[derive(Debug, Clone)]
pub struct BlockWireError {
    detail: String,
}

impl BlockWireError {
    /// Creates a wire failure with a human-readable detail.
    #[must_use]
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }

    /// Creates the default failure for a block that does not support
    /// [`BlockFactory::wire_input`] on the named port.
    #[must_use]
    pub fn unsupported(port: &StablePortKey) -> Self {
        Self::new(format!("block does not accept an input on port {port}"))
    }

    /// Returns the human-readable failure detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for BlockWireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for BlockWireError {}

/// How an embodiment registers one reusable block into a composition
/// catalogue.
///
/// `build` adds this block's node(s) to a graph under construction and
/// returns the [`NodeId`] other blocks wire to as this block's externally
/// addressable output. `wire_input` is called on the *reading* block's
/// factory, because only that factory knows the concrete Rust type its
/// input requires; it performs the typed `graph.read_from::<T>(reader,
/// producer)` call itself. A block that accepts no wired input keeps the
/// default implementation, which refuses every port.
///
/// A block referenced from a `Guard` gate must add its returned [`NodeId`]
/// as exactly one native system node: the materializer attaches `Guard`'s
/// timeout and retry policy with the native `set_timeout`/`set_retry_policy`
/// builder methods, and both panic if the node they are given is not a
/// system node.
pub trait BlockFactory: Send + Sync {
    /// Adds this block's node(s) to `graph`, using `config` as its resolved
    /// configuration, and returns the node other blocks wire to.
    ///
    /// # Errors
    ///
    /// Returns [`BlockBuildError`] if `config` does not satisfy this block's
    /// declared configuration schema, or the block cannot be added for any
    /// other reason.
    fn build(
        &self,
        graph: &mut Graph,
        config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError>;

    /// Reads `producer`'s output on the named stable port into `reader`'s
    /// input, using this factory's knowledge of `reader`'s concrete type.
    ///
    /// Called on the *reading* block's factory with `reader` set to the
    /// node this same factory built.
    ///
    /// # Errors
    ///
    /// Returns [`BlockWireError`] if this block does not accept an input on
    /// `port`. The default implementation always returns this error.
    fn wire_input(
        &self,
        graph: &mut Graph,
        reader: NodeId,
        producer: NodeId,
        port: &StablePortKey,
    ) -> Result<(), BlockWireError> {
        let _ = (graph, reader, producer);
        Err(BlockWireError::unsupported(port))
    }
}
