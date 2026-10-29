//! Turns a planner's gate-expression plan into a native Polaris `Graph`.
//!
//! A [`GatePlan`](plan::GatePlan) is data: a tree over a fixed set of gates
//! (step, sequence, branch, switch, fork, loop, guard, seal, slot) plus
//! plan-level wiring, naming only catalogue keys and configuration — never
//! code, closures, serialized graphs, or native node identities. An
//! embodiment builds a [`CompositionCatalogue`](catalogue::CompositionCatalogue)
//! of reusable [`BlockFactory`](factory::BlockFactory) implementations and
//! registered predicates, discriminators, selectors, handler recipes, and
//! policies, and seals it once at startup. The host checks a submitted plan
//! against host-supplied [`CompositionLimits`](limits::CompositionLimits)
//! and the catalogue's declared block [`Effect`](effects::Effect)s, then
//! calls [`materialize`](materializer::materialize) to build and validate an
//! ordinary native graph — or collects [`Refusal`](refusal::Refusal)s the
//! planner can act on to submit a revised plan.
//!
//! This module is domain-neutral: it knows nothing about any embodiment's
//! concrete systems, tools, or resources. Those live behind
//! [`BlockFactory`] implementations an embodiment registers into its own
//! catalogue.

pub mod catalogue;
pub mod description;
pub mod effects;
pub mod factory;
pub mod identity;
pub mod limits;
pub mod materializer;
pub mod plan;
pub mod refusal;

pub use catalogue::{
    CompositionCatalogue, CompositionCatalogueError, CompositionCatalogueSnapshot, RetentionPolicy,
    TimeoutPolicyEntry,
};
pub use description::BlockDescription;
pub use effects::Effect;
pub use factory::{BlockBuildError, BlockFactory, BlockWireError};
pub use identity::{
    BlockInstanceId, CaseKey, ConfigId, ContextPolicyId, ContextPolicyKey, DiscriminatorId,
    DiscriminatorKey, FactKey, HandlerId, HandlerKey, PredicateId, PredicateKey, RetentionPolicyId,
    RetentionPolicyKey, RetryPolicyId, RetryPolicyKey, StablePortId, StablePortKey,
    TimeoutPolicyId, TimeoutPolicyKey,
};
pub use limits::CompositionLimits;
pub use materializer::materialize;
pub use plan::{GateExpr, GatePlan, LoopControl, StepPlan, SwitchCase, WirePlan};
pub use refusal::{GatePath, GateStep, Refusal, RefusedKey};
