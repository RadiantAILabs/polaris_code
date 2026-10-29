//! The sealed registry a gate plan resolves its keys against.
//!
//! An embodiment builds a [`CompositionCatalogue`], registers its blocks,
//! predicates, discriminators, selectors, handler recipes, and policies, and
//! seals it once at startup. Sealing freezes the registry and computes a
//! stable digest over its declared shape, so a [`GatePlan`](super::GatePlan)
//! can name the exact catalogue it was planned against and the host can
//! detect a plan resolved against a stale one.
//!
//! Contracts referenced by `Seal` and `Slot` are not owned here: they come
//! from the existing topology contract catalogue, resolved separately by the
//! materializer.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use polaris::graph::predicate::{BoxedDiscriminator, BoxedPredicate};
use polaris::graph::{BoxedSelector, ContextPolicy, RetryPolicy};
use polaris::system::plugin::Version;
use serde::Serialize;

use super::description::BlockDescription;
use super::effects::Effect;
use super::factory::BlockFactory;
use super::identity::{
    CaseKey, ContextPolicyKey, DiscriminatorKey, FactKey, HandlerKey, PredicateKey,
    RetentionPolicyKey, RetryPolicyKey, StablePortKey, TimeoutPolicyKey,
};
use super::plan::GateExpr;
use crate::contracts::{
    CompositionBlockKey, CompositionCatalogId, CompositionCatalogKey, ConfigurationSchemaKey,
    ContentDigest, ContractError, SelectorKey,
};

struct BlockEntry {
    factory: Arc<dyn BlockFactory>,
    config_schema: ConfigurationSchemaKey,
    effect: Effect,
    description: Option<BlockDescription>,
}

struct DiscriminatorEntry {
    discriminator: Arc<BoxedDiscriminator>,
    cases: BTreeSet<CaseKey>,
}

/// A registered timeout policy: how long to wait, and the recipe to run if
/// the wait expires.
#[derive(Debug, Clone)]
pub struct TimeoutPolicyEntry {
    duration: Duration,
    handler: Option<GateExpr>,
}

impl TimeoutPolicyEntry {
    /// Creates a timeout policy entry.
    #[must_use]
    pub const fn new(duration: Duration, handler: Option<GateExpr>) -> Self {
        Self { duration, handler }
    }

    /// Returns the duration to wait before the step is considered timed out.
    #[must_use]
    pub const fn duration(&self) -> Duration {
        self.duration
    }

    /// Returns the recipe run when the step times out, if any.
    #[must_use]
    pub const fn handler(&self) -> Option<&GateExpr> {
        self.handler.as_ref()
    }
}

/// A registered loop retention policy: how many of a loop's most recent
/// completed iterations to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    iterations: u32,
}

impl RetentionPolicy {
    /// Keeps only the most recent `iterations` completed iterations.
    #[must_use]
    pub const fn keep_last(iterations: u32) -> Self {
        Self { iterations }
    }

    /// Returns the number of completed iterations kept.
    #[must_use]
    pub const fn iterations(self) -> u32 {
        self.iterations
    }
}

/// Registration or sealing failure for a [`CompositionCatalogue`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompositionCatalogueError {
    /// A mutation was attempted after sealing.
    Sealed,
    /// A block identity and version is already registered.
    DuplicateBlock(CompositionBlockKey),
    /// A description was submitted for a block that was never registered.
    UnknownBlock(CompositionBlockKey),
    /// A block already carries a description.
    DuplicateDescription(CompositionBlockKey),
    /// A predicate identity and version is already registered.
    DuplicatePredicate(PredicateKey),
    /// A discriminator identity and version is already registered.
    DuplicateDiscriminator(DiscriminatorKey),
    /// A selector identity and version is already registered.
    DuplicateSelector(SelectorKey),
    /// A handler identity and version is already registered.
    DuplicateHandler(HandlerKey),
    /// A timeout policy identity and version is already registered.
    DuplicateTimeoutPolicy(TimeoutPolicyKey),
    /// A retry policy identity and version is already registered.
    DuplicateRetryPolicy(RetryPolicyKey),
    /// A retention policy identity and version is already registered.
    DuplicateRetentionPolicy(RetentionPolicyKey),
    /// A context policy identity and version is already registered.
    DuplicateContextPolicy(ContextPolicyKey),
    /// A discriminator was registered with an empty closed case-key set.
    EmptyCaseSet(DiscriminatorKey),
    /// A handler or timeout-policy recipe failed structural validation.
    InvalidRecipe {
        /// The handler identity whose recipe was invalid.
        handler: HandlerKey,
        /// The structural validation failure.
        source: ContractError,
    },
}

impl std::fmt::Display for CompositionCatalogueError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sealed => formatter.write_str("catalogue is sealed"),
            Self::DuplicateBlock(key) => write!(formatter, "duplicate block {key}"),
            Self::UnknownBlock(key) => {
                write!(formatter, "block {key} is not registered")
            }
            Self::DuplicateDescription(key) => {
                write!(formatter, "block {key} already has a description")
            }
            Self::DuplicatePredicate(key) => write!(formatter, "duplicate predicate {key}"),
            Self::DuplicateDiscriminator(key) => write!(formatter, "duplicate discriminator {key}"),
            Self::DuplicateSelector(key) => write!(formatter, "duplicate selector {key}"),
            Self::DuplicateHandler(key) => write!(formatter, "duplicate handler {key}"),
            Self::DuplicateTimeoutPolicy(key) => {
                write!(formatter, "duplicate timeout policy {key}")
            }
            Self::DuplicateRetryPolicy(key) => write!(formatter, "duplicate retry policy {key}"),
            Self::DuplicateRetentionPolicy(key) => {
                write!(formatter, "duplicate retention policy {key}")
            }
            Self::DuplicateContextPolicy(key) => {
                write!(formatter, "duplicate context policy {key}")
            }
            Self::EmptyCaseSet(key) => {
                write!(formatter, "discriminator {key} declares no case keys")
            }
            Self::InvalidRecipe { handler, source } => {
                write!(formatter, "handler {handler} recipe is invalid: {source}")
            }
        }
    }
}

impl std::error::Error for CompositionCatalogueError {}

/// A registry of block factories and registered predicates, discriminators,
/// selectors, handler recipes, and policies, open for registration until
/// [`seal`](Self::seal) is called.
pub struct CompositionCatalogue {
    id: CompositionCatalogId,
    version: Version,
    blocks: BTreeMap<CompositionBlockKey, BlockEntry>,
    predicates: BTreeMap<PredicateKey, BoxedPredicate>,
    discriminators: BTreeMap<DiscriminatorKey, DiscriminatorEntry>,
    selectors: BTreeMap<SelectorKey, BoxedSelector>,
    handlers: BTreeMap<HandlerKey, GateExpr>,
    timeout_policies: BTreeMap<TimeoutPolicyKey, TimeoutPolicyEntry>,
    retry_policies: BTreeMap<RetryPolicyKey, RetryPolicy>,
    retention_policies: BTreeMap<RetentionPolicyKey, RetentionPolicy>,
    context_policies: BTreeMap<ContextPolicyKey, ContextPolicy>,
}

impl CompositionCatalogue {
    /// Creates an empty catalogue under construction.
    #[must_use]
    pub fn new(id: CompositionCatalogId, version: Version) -> Self {
        Self {
            id,
            version,
            blocks: BTreeMap::new(),
            predicates: BTreeMap::new(),
            discriminators: BTreeMap::new(),
            selectors: BTreeMap::new(),
            handlers: BTreeMap::new(),
            timeout_policies: BTreeMap::new(),
            retry_policies: BTreeMap::new(),
            retention_policies: BTreeMap::new(),
            context_policies: BTreeMap::new(),
        }
    }

    /// Registers a reusable block behind a stable, versioned identity.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionCatalogueError::DuplicateBlock`] if `key` is
    /// already registered.
    pub fn register_block(
        &mut self,
        key: CompositionBlockKey,
        factory: Arc<dyn BlockFactory>,
        config_schema: ConfigurationSchemaKey,
        effect: Effect,
    ) -> Result<&mut Self, CompositionCatalogueError> {
        if self.blocks.contains_key(&key) {
            return Err(CompositionCatalogueError::DuplicateBlock(key));
        }
        self.blocks.insert(
            key,
            BlockEntry {
                factory,
                config_schema,
                effect,
                description: None,
            },
        );
        Ok(self)
    }

    /// Attaches discoverable metadata to an already-registered block.
    ///
    /// A block need not be described to be materializable. Call this once per
    /// block, in the same registration pass as
    /// [`register_block`](Self::register_block).
    ///
    /// # Errors
    ///
    /// Returns [`CompositionCatalogueError::UnknownBlock`] if `key` was never
    /// registered, or [`CompositionCatalogueError::DuplicateDescription`] if
    /// it already carries one.
    pub fn describe_block(
        &mut self,
        key: &CompositionBlockKey,
        description: BlockDescription,
    ) -> Result<&mut Self, CompositionCatalogueError> {
        let entry = self
            .blocks
            .get_mut(key)
            .ok_or_else(|| CompositionCatalogueError::UnknownBlock(key.clone()))?;
        if entry.description.is_some() {
            return Err(CompositionCatalogueError::DuplicateDescription(key.clone()));
        }
        entry.description = Some(description);
        Ok(self)
    }

    /// Registers a branch predicate behind a stable, versioned identity.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionCatalogueError::DuplicatePredicate`] if `key` is
    /// already registered.
    pub fn register_predicate(
        &mut self,
        key: PredicateKey,
        predicate: BoxedPredicate,
    ) -> Result<&mut Self, CompositionCatalogueError> {
        if self.predicates.contains_key(&key) {
            return Err(CompositionCatalogueError::DuplicatePredicate(key));
        }
        self.predicates.insert(key, predicate);
        Ok(self)
    }

    /// Registers a `Switch` discriminator and its closed set of case keys.
    ///
    /// A `Switch` naming this discriminator must cover every key in `cases`,
    /// or declare a default: the discriminator's key set is closed, and the
    /// materializer refuses an incomplete `Switch` before running anything,
    /// since the native graph builder only catches it at run time.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionCatalogueError::DuplicateDiscriminator`] if `key`
    /// is already registered, or
    /// [`CompositionCatalogueError::EmptyCaseSet`] if `cases` is empty.
    pub fn register_discriminator(
        &mut self,
        key: DiscriminatorKey,
        discriminator: BoxedDiscriminator,
        cases: BTreeSet<CaseKey>,
    ) -> Result<&mut Self, CompositionCatalogueError> {
        if self.discriminators.contains_key(&key) {
            return Err(CompositionCatalogueError::DuplicateDiscriminator(key));
        }
        if cases.is_empty() {
            return Err(CompositionCatalogueError::EmptyCaseSet(key));
        }
        self.discriminators.insert(
            key,
            DiscriminatorEntry {
                discriminator: Arc::new(discriminator),
                cases,
            },
        );
        Ok(self)
    }

    /// Registers a `Slot` selector behind a stable, versioned identity.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionCatalogueError::DuplicateSelector`] if `key` is
    /// already registered.
    pub fn register_selector(
        &mut self,
        key: SelectorKey,
        selector: BoxedSelector,
    ) -> Result<&mut Self, CompositionCatalogueError> {
        if self.selectors.contains_key(&key) {
            return Err(CompositionCatalogueError::DuplicateSelector(key));
        }
        self.selectors.insert(key, selector);
        Ok(self)
    }

    /// Registers a `Guard` error-handler recipe.
    ///
    /// `recipe` must satisfy the same structural rules as any gate
    /// sub-expression (for example, no empty `Seq`); it is validated
    /// immediately.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionCatalogueError::DuplicateHandler`] if `key` is
    /// already registered, or
    /// [`CompositionCatalogueError::InvalidRecipe`] if `recipe` fails
    /// structural validation.
    pub fn register_handler(
        &mut self,
        key: HandlerKey,
        recipe: GateExpr,
    ) -> Result<&mut Self, CompositionCatalogueError> {
        if self.handlers.contains_key(&key) {
            return Err(CompositionCatalogueError::DuplicateHandler(key));
        }
        recipe
            .validate_shape()
            .map_err(|source| CompositionCatalogueError::InvalidRecipe {
                handler: key.clone(),
                source,
            })?;
        self.handlers.insert(key, recipe);
        Ok(self)
    }

    /// Registers a timeout policy, with an optional handler recipe run when
    /// the wait expires.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionCatalogueError::DuplicateTimeoutPolicy`] if `key`
    /// is already registered.
    pub fn register_timeout_policy(
        &mut self,
        key: TimeoutPolicyKey,
        entry: TimeoutPolicyEntry,
    ) -> Result<&mut Self, CompositionCatalogueError> {
        if self.timeout_policies.contains_key(&key) {
            return Err(CompositionCatalogueError::DuplicateTimeoutPolicy(key));
        }
        self.timeout_policies.insert(key, entry);
        Ok(self)
    }

    /// Registers a retry policy.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionCatalogueError::DuplicateRetryPolicy`] if `key`
    /// is already registered.
    pub fn register_retry_policy(
        &mut self,
        key: RetryPolicyKey,
        policy: RetryPolicy,
    ) -> Result<&mut Self, CompositionCatalogueError> {
        if self.retry_policies.contains_key(&key) {
            return Err(CompositionCatalogueError::DuplicateRetryPolicy(key));
        }
        self.retry_policies.insert(key, policy);
        Ok(self)
    }

    /// Registers a loop retention policy.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionCatalogueError::DuplicateRetentionPolicy`] if
    /// `key` is already registered.
    pub fn register_retention_policy(
        &mut self,
        key: RetentionPolicyKey,
        policy: RetentionPolicy,
    ) -> Result<&mut Self, CompositionCatalogueError> {
        if self.retention_policies.contains_key(&key) {
            return Err(CompositionCatalogueError::DuplicateRetentionPolicy(key));
        }
        self.retention_policies.insert(key, policy);
        Ok(self)
    }

    /// Registers a scope context policy.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionCatalogueError::DuplicateContextPolicy`] if `key`
    /// is already registered.
    pub fn register_context_policy(
        &mut self,
        key: ContextPolicyKey,
        policy: ContextPolicy,
    ) -> Result<&mut Self, CompositionCatalogueError> {
        if self.context_policies.contains_key(&key) {
            return Err(CompositionCatalogueError::DuplicateContextPolicy(key));
        }
        self.context_policies.insert(key, policy);
        Ok(self)
    }

    /// Freezes registration and computes the catalogue's stable digest.
    ///
    /// # Errors
    ///
    /// Returns a [`ContractError`] if the digest cannot be computed.
    pub fn seal(self) -> Result<CompositionCatalogueSnapshot, ContractError> {
        let catalogue_key = CompositionCatalogKey::new(self.id, self.version);
        let identity = CompositionCatalogueIdentity {
            catalogue: &catalogue_key,
            blocks: self
                .blocks
                .iter()
                .map(|(key, entry)| BlockIdentity {
                    block: key,
                    config_schema: &entry.config_schema,
                    effect: entry.effect,
                    description: entry.description.as_ref().map(|description| {
                        BlockDescriptionIdentity {
                            summary: description.summary(),
                            inputs: description.inputs(),
                            output: description.output(),
                            required_facts: description.required_facts().iter().collect(),
                            example: description.example(),
                        }
                    }),
                })
                .collect(),
            predicates: self.predicates.keys().collect(),
            discriminators: self
                .discriminators
                .iter()
                .map(|(key, entry)| DiscriminatorIdentity {
                    discriminator: key,
                    cases: entry.cases.iter().collect(),
                })
                .collect(),
            selectors: self.selectors.keys().collect(),
            handlers: self.handlers.iter().collect(),
            timeout_policies: self
                .timeout_policies
                .iter()
                .map(|(key, entry)| TimeoutPolicyIdentity {
                    timeout_policy: key,
                    duration: entry.duration,
                    handler: entry.handler.as_ref(),
                })
                .collect(),
            retry_policies: self.retry_policies.keys().collect(),
            retention_policies: self
                .retention_policies
                .iter()
                .map(|(key, policy)| RetentionPolicyIdentity {
                    retention_policy: key,
                    iterations: policy.iterations,
                })
                .collect(),
            context_policies: self.context_policies.keys().collect(),
        };
        let digest = crate::contracts::content_digest(&identity)?;
        Ok(CompositionCatalogueSnapshot {
            catalogue: catalogue_key,
            digest,
            blocks: self
                .blocks
                .into_iter()
                .map(|(k, v)| (k, Arc::new(v)))
                .collect(),
            predicates: self
                .predicates
                .into_iter()
                .map(|(k, v)| (k, Arc::new(v)))
                .collect(),
            discriminators: self
                .discriminators
                .into_iter()
                .map(|(k, v)| (k, Arc::new(v)))
                .collect(),
            selectors: self
                .selectors
                .into_iter()
                .map(|(k, v)| (k, Arc::new(v)))
                .collect(),
            handlers: Arc::new(self.handlers),
            timeout_policies: Arc::new(self.timeout_policies),
            retry_policies: Arc::new(self.retry_policies),
            retention_policies: Arc::new(self.retention_policies),
            context_policies: self
                .context_policies
                .into_iter()
                .map(|(k, v)| (k, Arc::new(v)))
                .collect(),
        })
    }
}

#[derive(Serialize)]
struct BlockDescriptionIdentity<'a> {
    summary: &'a str,
    inputs: &'a [StablePortKey],
    output: &'a StablePortKey,
    required_facts: Vec<&'a FactKey>,
    example: &'a str,
}

#[derive(Serialize)]
struct BlockIdentity<'a> {
    block: &'a CompositionBlockKey,
    config_schema: &'a ConfigurationSchemaKey,
    effect: Effect,
    // A block's description is discoverable metadata a planner reads to
    // decide what is worth naming in a plan, exactly as a topology
    // candidate's descriptor (title, summary, claimed contracts) already
    // participates in the topology catalog's own digest
    // (`topology::catalog::catalog_content_digest`). Including it here keeps
    // the same invariant true for the composition catalogue: a plan
    // resolved against a stale digest is one whose author may have read
    // descriptions that no longer match what is registered, not just a plan
    // whose structural keys or schemas moved.
    description: Option<BlockDescriptionIdentity<'a>>,
}

#[derive(Serialize)]
struct DiscriminatorIdentity<'a> {
    discriminator: &'a DiscriminatorKey,
    cases: Vec<&'a CaseKey>,
}

#[derive(Serialize)]
struct TimeoutPolicyIdentity<'a> {
    timeout_policy: &'a TimeoutPolicyKey,
    duration: Duration,
    handler: Option<&'a GateExpr>,
}

#[derive(Serialize)]
struct RetentionPolicyIdentity<'a> {
    retention_policy: &'a RetentionPolicyKey,
    iterations: u32,
}

#[derive(Serialize)]
struct CompositionCatalogueIdentity<'a> {
    catalogue: &'a CompositionCatalogKey,
    blocks: Vec<BlockIdentity<'a>>,
    predicates: Vec<&'a PredicateKey>,
    discriminators: Vec<DiscriminatorIdentity<'a>>,
    selectors: Vec<&'a SelectorKey>,
    handlers: Vec<(&'a HandlerKey, &'a GateExpr)>,
    timeout_policies: Vec<TimeoutPolicyIdentity<'a>>,
    retry_policies: Vec<&'a RetryPolicyKey>,
    retention_policies: Vec<RetentionPolicyIdentity<'a>>,
    context_policies: Vec<&'a ContextPolicyKey>,
}

/// An immutable, sealed composition catalogue snapshot.
///
/// A [`GatePlan`](super::GatePlan) names the [`CompositionCatalogKey`] and
/// [`ContentDigest`] of the snapshot it was planned against; the host checks
/// both before materializing.
#[derive(Clone)]
pub struct CompositionCatalogueSnapshot {
    catalogue: CompositionCatalogKey,
    digest: ContentDigest,
    blocks: BTreeMap<CompositionBlockKey, Arc<BlockEntry>>,
    predicates: BTreeMap<PredicateKey, Arc<BoxedPredicate>>,
    discriminators: BTreeMap<DiscriminatorKey, Arc<DiscriminatorEntry>>,
    selectors: BTreeMap<SelectorKey, Arc<BoxedSelector>>,
    handlers: Arc<BTreeMap<HandlerKey, GateExpr>>,
    timeout_policies: Arc<BTreeMap<TimeoutPolicyKey, TimeoutPolicyEntry>>,
    retry_policies: Arc<BTreeMap<RetryPolicyKey, RetryPolicy>>,
    retention_policies: Arc<BTreeMap<RetentionPolicyKey, RetentionPolicy>>,
    context_policies: BTreeMap<ContextPolicyKey, Arc<ContextPolicy>>,
}

impl CompositionCatalogueSnapshot {
    /// Returns the sealed catalogue's stable identity and version.
    #[must_use]
    pub const fn catalogue(&self) -> &CompositionCatalogKey {
        &self.catalogue
    }

    /// Returns the digest over the catalogue's declared shape.
    #[must_use]
    pub const fn digest(&self) -> ContentDigest {
        self.digest
    }

    /// Looks up a registered block's factory, configuration schema, and
    /// declared effect.
    #[must_use]
    pub fn block(
        &self,
        key: &CompositionBlockKey,
    ) -> Option<(&Arc<dyn BlockFactory>, &ConfigurationSchemaKey, Effect)> {
        self.blocks
            .get(key)
            .map(|entry| (&entry.factory, &entry.config_schema, entry.effect))
    }

    /// Looks up a registered block's description, if it was given one.
    #[must_use]
    pub fn block_description(&self, key: &CompositionBlockKey) -> Option<&BlockDescription> {
        self.blocks
            .get(key)
            .and_then(|entry| entry.description.as_ref())
    }

    /// Lists every described block whose required facts `satisfied` accepts,
    /// in registered key order.
    ///
    /// A block with no description is never admissible: it has declared
    /// nothing a host can check. `satisfied` is asked about every fact a
    /// candidate block requires, so a host with zero known facts can pass
    /// `|_| false` to get back only blocks that require nothing.
    #[must_use]
    pub fn admissible_blocks(
        &self,
        mut satisfied: impl FnMut(&FactKey) -> bool,
    ) -> Vec<(&CompositionBlockKey, &BlockDescription)> {
        self.blocks
            .iter()
            .filter_map(|(key, entry)| {
                let description = entry.description.as_ref()?;
                description
                    .required_facts()
                    .iter()
                    .all(&mut satisfied)
                    .then_some((key, description))
            })
            .collect()
    }

    /// Looks up a registered predicate.
    ///
    /// Returns an owned, cloned handle rather than a borrow: a plan may
    /// reference the same registered predicate at more than one tree
    /// position, and the materializer mints one fresh native
    /// [`BoxedPredicate`] per reference from this shared handle, since native
    /// builder methods consume a `BoxedPredicate` by value.
    #[must_use]
    pub fn predicate(&self, key: &PredicateKey) -> Option<Arc<BoxedPredicate>> {
        self.predicates.get(key).cloned()
    }

    /// Looks up a registered discriminator and its closed case-key set.
    ///
    /// Returns an owned, cloned handle for the same reason as
    /// [`predicate`](Self::predicate).
    #[must_use]
    pub fn discriminator(
        &self,
        key: &DiscriminatorKey,
    ) -> Option<(Arc<BoxedDiscriminator>, &BTreeSet<CaseKey>)> {
        self.discriminators
            .get(key)
            .map(|entry| (Arc::clone(&entry.discriminator), &entry.cases))
    }

    /// Looks up a registered selector.
    ///
    /// Returns an owned, cloned handle for the same reason as
    /// [`predicate`](Self::predicate).
    #[must_use]
    pub fn selector(&self, key: &SelectorKey) -> Option<Arc<BoxedSelector>> {
        self.selectors.get(key).cloned()
    }

    /// Looks up a registered handler recipe.
    #[must_use]
    pub fn handler(&self, key: &HandlerKey) -> Option<&GateExpr> {
        self.handlers.get(key)
    }

    /// Looks up a registered timeout policy.
    #[must_use]
    pub fn timeout_policy(&self, key: &TimeoutPolicyKey) -> Option<&TimeoutPolicyEntry> {
        self.timeout_policies.get(key)
    }

    /// Looks up a registered retry policy.
    #[must_use]
    pub fn retry_policy(&self, key: &RetryPolicyKey) -> Option<&RetryPolicy> {
        self.retry_policies.get(key)
    }

    /// Looks up a registered loop retention policy.
    #[must_use]
    pub fn retention_policy(&self, key: &RetentionPolicyKey) -> Option<RetentionPolicy> {
        self.retention_policies.get(key).copied()
    }

    /// Looks up a registered scope context policy.
    #[must_use]
    pub fn context_policy(&self, key: &ContextPolicyKey) -> Option<&ContextPolicy> {
        self.context_policies.get(key).map(Arc::as_ref)
    }
}

impl std::fmt::Debug for CompositionCatalogueSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CompositionCatalogueSnapshot")
            .field("catalogue", &self.catalogue)
            .field("digest", &self.digest)
            .field("blocks", &self.blocks.len())
            .field("predicates", &self.predicates.len())
            .field("discriminators", &self.discriminators.len())
            .field("selectors", &self.selectors.len())
            .field("handlers", &self.handlers.len())
            .field("timeout_policies", &self.timeout_policies.len())
            .field("retry_policies", &self.retry_policies.len())
            .field("retention_policies", &self.retention_policies.len())
            .field("context_policies", &self.context_policies.len())
            .finish()
    }
}
