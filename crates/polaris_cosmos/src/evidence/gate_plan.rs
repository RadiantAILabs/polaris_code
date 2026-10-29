//! Gate-plan evidence keys and outcome indexes.
//!
//! The canonical plan digest identifies every configuration value. The shape
//! key deliberately does not: it retains the gate tree, registered
//! non-configuration keys, and plan-level wires, while ignoring plan-local
//! instance and configuration labels. This lets later policy compare
//! structural approaches without conflating a changed budget or instruction
//! with a new shape.

use std::collections::BTreeMap;

use crate::composition::{BlockInstanceId, GateExpr, GatePlan, StablePortKey};
use crate::contracts::{
    CanonicalRecord, CompositionOutcome, CompositionRevisionTrail, ContentDigest,
    GateExpressionShapeKey, GatePlanRecord,
};

/// Derives the durable gate-plan identity retained in composition evidence.
#[must_use]
pub fn gate_plan_record(plan: &GatePlan) -> GatePlanRecord {
    GatePlanRecord::new(
        plan.plan_digest(),
        plan.catalogue_digest(),
        gate_plan_shape(plan),
    )
}

/// Derives a canonical gate-expression shape independent of configuration values.
///
/// A gate expression names configuration and instance labels solely to connect
/// it to the plan-level configuration table and wiring. Neither changes the
/// expression's control-flow shape, so both labels are omitted before
/// canonical hashing.
#[must_use]
pub fn gate_expression_shape(expression: &GateExpr) -> GateExpressionShapeKey {
    let mut expression = serde_json::to_value(expression)
        .expect("gate expressions are serializable by their public contract");
    normalize_shape(&mut expression);
    shape_digest(&expression)
}

/// Derives a canonical gate-plan shape independent of configuration values.
///
/// The projection retains plan-level wires because they become native graph
/// edges. It replaces the reader and producer labels in every wire with the
/// stable path of the corresponding step in the expression tree, so renaming
/// instances does not change shape while changing a wire endpoint or port
/// does.
#[must_use]
pub fn gate_plan_shape(plan: &GatePlan) -> GateExpressionShapeKey {
    let mut expression = serde_json::to_value(plan.expression())
        .expect("gate expressions are serializable by their public contract");
    normalize_shape(&mut expression);
    let mut paths = Vec::new();
    collect_instance_paths(plan.expression(), "root", &mut paths);
    let projection = ShapeProjection {
        expression,
        wires: plan
            .wires()
            .iter()
            .map(|wire| ShapeWire {
                reader: instance_path(wire.reader(), &paths),
                producer: instance_path(wire.producer(), &paths),
                port: wire.port(),
            })
            .collect(),
    };
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&projection, &mut bytes)
        .expect("gate-plan shape projections are canonically CBOR-serializable");
    GateExpressionShapeKey::new(ContentDigest::sha256(&bytes))
}

fn shape_digest(value: &serde_json::Value) -> GateExpressionShapeKey {
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(value, &mut bytes)
        .expect("JSON values used for gate shapes are canonically CBOR-serializable");
    GateExpressionShapeKey::new(ContentDigest::sha256(&bytes))
}

#[derive(serde::Serialize)]
struct ShapeProjection<'a> {
    expression: serde_json::Value,
    wires: Vec<ShapeWire<'a>>,
}

#[derive(serde::Serialize)]
struct ShapeWire<'a> {
    reader: String,
    producer: String,
    port: &'a StablePortKey,
}

fn normalize_shape(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                normalize_shape(value);
            }
        }
        serde_json::Value::Object(values) => {
            values.remove("config");
            values.remove("instance");
            for value in values.values_mut() {
                normalize_shape(value);
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
}

fn collect_instance_paths(
    expression: &GateExpr,
    path: &str,
    paths: &mut Vec<(BlockInstanceId, String)>,
) {
    match expression {
        GateExpr::Step(step) => paths.push((step.instance().clone(), path.to_owned())),
        GateExpr::Seq(children) => {
            for (index, child) in children.iter().enumerate() {
                collect_instance_paths(child, &format!("{path}/seq/{index}"), paths);
            }
        }
        GateExpr::Choose {
            when_true,
            when_false,
            ..
        } => {
            collect_instance_paths(when_true, &format!("{path}/when-true"), paths);
            collect_instance_paths(when_false, &format!("{path}/when-false"), paths);
        }
        GateExpr::Switch { cases, default, .. } => {
            for (index, case) in cases.iter().enumerate() {
                collect_instance_paths(case.body(), &format!("{path}/case/{index}"), paths);
            }
            if let Some(default) = default {
                collect_instance_paths(default, &format!("{path}/default"), paths);
            }
        }
        GateExpr::Fork { arms, reducer } => {
            for (index, arm) in arms.iter().enumerate() {
                collect_instance_paths(arm, &format!("{path}/arm/{index}"), paths);
            }
            paths.push((reducer.instance().clone(), format!("{path}/reducer")));
        }
        GateExpr::Loop { body, .. }
        | GateExpr::Repeat { body, .. }
        | GateExpr::Seal { body, .. } => {
            collect_instance_paths(body, &format!("{path}/body"), paths);
        }
        GateExpr::Guard { step, .. } => {
            paths.push((step.instance().clone(), format!("{path}/step")));
        }
        GateExpr::Slot { candidates, .. } => {
            for (index, candidate) in candidates.iter().enumerate() {
                collect_instance_paths(candidate, &format!("{path}/candidate/{index}"), paths);
            }
        }
    }
}

fn instance_path(instance: &BlockInstanceId, paths: &[(BlockInstanceId, String)]) -> String {
    paths
        .iter()
        .find(|(candidate, _)| candidate == instance)
        .map_or_else(
            || format!("unresolved:{}", instance.as_str()),
            |(_, path)| path.clone(),
        )
}

/// Caller-owned in-memory query index over retained gate-plan outcomes.
///
/// This is deliberately an index, not an evidence provider: the host's
/// append-only provider remains authoritative for retention and lifecycle.
#[derive(Debug, Default)]
pub struct GatePlanOutcomeIndex {
    by_plan: BTreeMap<ContentDigest, Vec<CompositionOutcome>>,
    by_shape: BTreeMap<GateExpressionShapeKey, Vec<CompositionOutcome>>,
}

impl GatePlanOutcomeIndex {
    /// Adds the terminal outcome from one valid revision trail, if it has one.
    pub fn insert(
        &mut self,
        trail: &CompositionRevisionTrail,
    ) -> Result<(), crate::contracts::ContractError> {
        trail.validate()?;
        let Some(outcome) = trail.outcome() else {
            return Ok(());
        };
        let gate_plan = outcome
            .gate_plan()
            .expect("validated revision trails only admit outcomes linked to gate-plan records");
        self.by_plan
            .entry(gate_plan.plan_digest())
            .or_default()
            .push(outcome.clone());
        self.by_shape
            .entry(gate_plan.shape())
            .or_default()
            .push(outcome.clone());
        Ok(())
    }

    /// Returns retained outcomes for one complete plan digest.
    #[must_use]
    pub fn outcomes_by_plan(&self, plan_digest: ContentDigest) -> &[CompositionOutcome] {
        self.by_plan.get(&plan_digest).map_or(&[], Vec::as_slice)
    }

    /// Returns retained outcomes for one configuration-independent gate shape.
    #[must_use]
    pub fn outcomes_by_shape(&self, shape: GateExpressionShapeKey) -> &[CompositionOutcome] {
        self.by_shape.get(&shape).map_or(&[], Vec::as_slice)
    }
}
