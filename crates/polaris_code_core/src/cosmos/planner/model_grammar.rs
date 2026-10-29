//! Model-facing data grammar; Cosmos remains the structural validator.

use super::*;
use polaris_cosmos::composition::{
    CaseKey, DiscriminatorKey, LoopControl, PredicateKey, SwitchCase, WirePlan,
};
use polaris_cosmos::contracts::ConfigurationEntry;
use serde_json::{Value, json};

use crate::cosmos::domains::{Arm, Domain, DomainSelection};
use crate::cosmos::gate_blocks::{keep_last_iteration_key, predicates};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub(super) enum Expression {
    Step(String),
    Seq(Vec<Self>),
    Choose {
        predicate: String,
        when_true: Box<Self>,
        when_false: Box<Self>,
    },
    Switch {
        discriminator: String,
        cases: BTreeMap<String, Self>,
        #[serde(default)]
        default: Option<Box<Self>>,
    },
    Repeat {
        body: Box<Self>,
        /// Additional runs after the guaranteed first run.
        count: u16,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Wire {
    reader: String,
    producer: String,
    port: String,
}

impl Wire {
    pub(super) fn resolve(&self) -> Result<WirePlan, String> {
        let port = configurable::patch_intent_port_key();
        if self.port != port.id().as_str() {
            return Err(format!("unknown wire port '{}'", self.port));
        }
        Ok(WirePlan::new(
            BlockInstanceId::new(&self.reader).map_err(|e| e.to_string())?,
            BlockInstanceId::new(&self.producer).map_err(|e| e.to_string())?,
            port,
        ))
    }
}

fn predicates_for(selection: DomainSelection) -> Vec<(PredicateKey, &'static str)> {
    let mut entries = vec![
        (
            predicates::tests_passed_key(),
            "the upstream judged assessment passed",
        ),
        (
            predicates::review_approved_key(),
            "the upstream model review approved the change",
        ),
        (
            predicates::budget_left_key(),
            "the upstream sweep has remaining work and budget",
        ),
        (
            predicates::has_check_key(),
            "the upstream conversation proposal contains a check script",
        ),
        (
            predicates::check_needs_repair_key(),
            "the upstream captured check failed on behavior, not infrastructure",
        ),
    ];
    if selection.arm == Arm::Composed && selection.domain == Domain::Sphinx {
        entries.push((
            crate::cosmos::domains::sphinx::expert::blocks::has_verification_key(),
            "the upstream Sphinx verification plan has a submitted check or mirrored tests",
        ));
    }
    entries
}

fn discriminators_for(selection: DomainSelection) -> Vec<(DiscriminatorKey, &'static str)> {
    let mut entries = vec![
        (
            predicates::task_kind_key(),
            "routes the task by its declared success check",
        ),
        (
            predicates::task_disposition_key(),
            "routes the upstream classified request",
        ),
    ];
    if selection.arm == Arm::Composed && selection.domain == Domain::Sympy {
        entries.push((crate::cosmos::domains::sympy::expert::blocks::verification_state_key(),
            "routes the upstream SymPy verification result: passed, behavioral failure, or unavailable"));
    }
    entries
}

pub(super) fn prompt(
    catalogue: &CompositionCatalogueSnapshot,
    selection: DomainSelection,
) -> String {
    let patch_port = configurable::patch_intent_port_key();
    let predicates = predicates_for(selection)
        .into_iter()
        .filter(|(key, _)| catalogue.predicate(key).is_some())
        .map(|(key, meaning)| format!("{} ({meaning})", key.id().as_str()))
        .collect::<Vec<_>>()
        .join("; ");
    let discriminators = discriminators_for(selection)
        .into_iter()
        .filter_map(|(key, meaning)| {
            catalogue.discriminator(&key).map(|(_, cases)| {
                format!(
                    "{} ({meaning}; cases: {})",
                    key.id().as_str(),
                    cases
                        .iter()
                        .map(|c| c.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        " Optional expression is a tree of step(instance), seq, choose, switch, or repeat, using the tool schema. \
        Without expression, steps run in their original order. With expression, use every declared instance exactly once. \
        Choose and switch read typed upstream outputs; produce those before branching. Predicates: {predicates}. \
        Discriminators: {discriminators}. Repeat count means additional runs after the first (0..=256), retains the last iteration, \
        and only accepts repeat-safe bodies. Wires use reader/producer instance labels and port {} \
        (PatchIntent output into apply-patch-intent); producers must precede consumers on every path. \
        Wires cannot cross a Repeat boundary. Host limits: 64 instances, depth 12, 128 wires, 2 nested repeats, 256 native nodes. \
        Config for a custom-schema block is {{\"registered\":value}}: create-file takes {{\"path\":string,\"content\":string}}; \
        survey-paths takes a string array; scan-markers takes {{\"marker\":string,\"max_sites\":1..10000,\"paths\":[strings]}} \
        or root:true instead of paths; classify-request takes {{\"allowed_dispositions\":[\"edit\",\"answer\",\"clarify\"]}} \
        (a nonempty subset); request-clarification takes {{\"max_questions\":1..3}}. \
        model-propose-conversation and resume-model-propose take the same model-propose config as model-propose. \
        Resume requires an upstream conversation proposal and captured TestRunOutcome. No arbitrary expressions, ports, or schema keys.",
        patch_port.id().as_str()
    )
}

impl Expression {
    pub(super) fn resolve(
        &self,
        steps: &mut BTreeMap<String, StepPlan>,
        catalogue: &CompositionCatalogueSnapshot,
        selection: DomainSelection,
        depth: u32,
    ) -> Result<GateExpr, String> {
        if depth > CompositionLimits::conservative().max_depth {
            return Err("expression exceeds host depth limit".to_owned());
        }
        let resolve = |expr: &Self, steps: &mut BTreeMap<String, StepPlan>| {
            expr.resolve(steps, catalogue, selection, depth + 1)
        };
        Ok(match self {
            Self::Step(instance) => {
                GateExpr::Step(steps.remove(instance).ok_or_else(|| {
                    format!("unknown or repeated expression instance '{instance}'")
                })?)
            }
            Self::Seq(children) => GateExpr::Seq(
                children
                    .iter()
                    .map(|child| resolve(child, steps))
                    .collect::<Result<_, _>>()?,
            ),
            Self::Choose {
                predicate,
                when_true,
                when_false,
            } => {
                let key = predicates_for(selection)
                    .into_iter()
                    .find(|(key, _)| {
                        key.id().as_str() == predicate && catalogue.predicate(key).is_some()
                    })
                    .ok_or_else(|| format!("unknown predicate '{predicate}'"))?
                    .0;
                GateExpr::Choose {
                    predicate: key,
                    when_true: Box::new(resolve(when_true, steps)?),
                    when_false: Box::new(resolve(when_false, steps)?),
                }
            }
            Self::Switch {
                discriminator,
                cases,
                default,
            } => {
                let key = discriminators_for(selection)
                    .into_iter()
                    .find(|(key, _)| {
                        key.id().as_str() == discriminator && catalogue.discriminator(key).is_some()
                    })
                    .ok_or_else(|| format!("unknown discriminator '{discriminator}'"))?
                    .0;
                GateExpr::Switch {
                    discriminator: key,
                    cases: cases
                        .iter()
                        .map(|(key, body)| {
                            Ok(SwitchCase::new(
                                CaseKey::new(key).map_err(|e| e.to_string())?,
                                resolve(body, steps)?,
                            ))
                        })
                        .collect::<Result<_, String>>()?,
                    default: default
                        .as_ref()
                        .map(|body| resolve(body, steps).map(Box::new))
                        .transpose()?,
                }
            }
            Self::Repeat { body, count } => GateExpr::Repeat {
                body: Box::new(resolve(body, steps)?),
                control: LoopControl::Count(*count),
                retention: keep_last_iteration_key(),
            },
        })
    }
}

pub(super) fn configuration(value: &Value) -> Result<ConfigurationValue, String> {
    Ok(match value {
        Value::Null => ConfigurationValue::Null,
        Value::Bool(v) => ConfigurationValue::Bool(*v),
        Value::String(v) => ConfigurationValue::String(v.clone()),
        Value::Number(v) => {
            if let Some(v) = v.as_u64() {
                ConfigurationValue::Unsigned(v)
            } else {
                ConfigurationValue::Signed(
                    v.as_i64().ok_or("configuration numbers must be integers")?,
                )
            }
        }
        Value::Array(v) => {
            ConfigurationValue::List(v.iter().map(configuration).collect::<Result<_, _>>()?)
        }
        Value::Object(v) => ConfigurationValue::map(
            v.iter()
                .map(|(key, v)| {
                    ConfigurationEntry::new(key, configuration(v)?).map_err(|e| e.to_string())
                })
                .collect::<Result<_, String>>()?,
        )
        .map_err(|e| e.to_string())?,
    })
}

pub(super) fn extend_schema(schema: &mut Value) {
    let expr = json!({"$ref":"#/$defs/expression"});
    let patch_port = configurable::patch_intent_port_key();
    schema["properties"]["expression"] = expr.clone();
    schema["properties"]["steps"]["maxItems"] = json!(64);
    schema["properties"]["wires"] = json!({"type":"array","maxItems":128,"items":{
        "type":"object","properties":{"reader":{"type":"string"},"producer":{"type":"string"},"port":{"enum":[patch_port.id().as_str()]}},
        "required":["reader","producer","port"],"additionalProperties":false}});
    schema["properties"]["steps"]["items"]["properties"]["config"]["anyOf"].as_array_mut().unwrap().push(json!({
        "type":"object","properties":{"registered":{}},"required":["registered"],"additionalProperties":false}));
    schema["$defs"] = json!({"expression":{"oneOf":[
        {"type":"object","properties":{"step":{"type":"string"}},"required":["step"],"additionalProperties":false},
        {"type":"object","properties":{"seq":{"type":"array","minItems":1,"maxItems":64,"items":expr}},"required":["seq"],"additionalProperties":false},
        {"type":"object","properties":{"choose":{"type":"object","properties":{
            "predicate":{"type":"string"},"when_true":expr,"when_false":expr},
            "required":["predicate","when_true","when_false"],"additionalProperties":false}},"required":["choose"],"additionalProperties":false},
        {"type":"object","properties":{"switch":{"type":"object","properties":{
            "discriminator":{"type":"string"},"cases":{"type":"object","additionalProperties":expr},"default":expr},
            "required":["discriminator","cases"],"additionalProperties":false}},"required":["switch"],"additionalProperties":false},
        {"type":"object","properties":{"repeat":{"type":"object","properties":{
            "body":expr,"count":{"type":"integer","minimum":0,"maximum":256}},
            "required":["body","count"],"additionalProperties":false}},"required":["repeat"],"additionalProperties":false}
    ]}});
}
