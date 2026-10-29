//! Read-only request classification and clarification blocks.
//!
//! These blocks make an underspecified coding request an explicit branch in a
//! gate plan. They do not guess an edit: an unservable request becomes a
//! bounded question and an escalated assessment conclusion.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use polaris::graph::{Graph, NodeId};
use polaris::system::param::{Out, Res};
use polaris::system::resource::LocalResource;
use polaris::system::system;
use polaris::system::system::SystemError;
use polaris_cosmos::composition::{
    BlockBuildError, BlockDescription, BlockFactory, CompositionCatalogue,
    CompositionCatalogueError, Effect,
};
use polaris_cosmos::contracts::{
    AssessmentConclusion, CompositionBlockId, CompositionBlockKey, ConfigurationEntry,
    ConfigurationSchemaId, ConfigurationSchemaKey, ConfigurationValue, ResolvedConfiguration,
    ResolvedConfigurationRef, Status, StopReason,
};

use super::super::assess::{conclude_findings, finding_assessment};
use super::super::block_cards;
use super::super::payload::QuestionFinding;
use super::super::task::{CodingIntent, CodingTask, SuccessCheck};
use super::behavior_version;

fn describe(
    summary: &str,
    inputs: Vec<polaris_cosmos::composition::StablePortKey>,
    output: polaris_cosmos::composition::StablePortKey,
    example: &str,
) -> BlockDescription {
    BlockDescription::new(summary, inputs, output, example)
        .expect("block description is well-formed")
}

const MAX_QUESTIONS_LIMIT: usize = 3;

fn exec(error: impl fmt::Display) -> SystemError {
    SystemError::ExecutionError(error.to_string())
}

fn block_key(id: &str) -> CompositionBlockKey {
    CompositionBlockKey::new(
        CompositionBlockId::new(id).expect("block ID is a valid stable name"),
        behavior_version(),
    )
}

fn schema_key(id: &str) -> ConfigurationSchemaKey {
    ConfigurationSchemaKey::new(
        ConfigurationSchemaId::new(id).expect("schema ID is a valid stable name"),
        behavior_version(),
    )
}

fn map(config: &ResolvedConfigurationRef) -> Result<&[ConfigurationEntry], BlockBuildError> {
    match config.resolved() {
        ResolvedConfiguration::Inline(ConfigurationValue::Map(entries)) => Ok(entries),
        ResolvedConfiguration::Inline(_) => {
            Err(BlockBuildError::new("expected a configuration map"))
        }
        ResolvedConfiguration::ContentAddressed(_) => Err(BlockBuildError::new(
            "content-addressed configuration is not supported",
        )),
    }
}

fn exact_keys(entries: &[ConfigurationEntry], expected: &[&str]) -> Result<(), BlockBuildError> {
    if entries.len() != expected.len()
        || entries.iter().any(|entry| !expected.contains(&entry.key()))
    {
        return Err(BlockBuildError::new(
            "configuration has unknown or missing fields",
        ));
    }
    Ok(())
}

fn required<'a>(
    entries: &'a [ConfigurationEntry],
    key: &str,
) -> Result<&'a ConfigurationValue, BlockBuildError> {
    entries
        .iter()
        .find(|entry| entry.key() == key)
        .map(ConfigurationEntry::value)
        .ok_or_else(|| BlockBuildError::new(format!("configuration is missing '{key}'")))
}

/// A deterministic route for a coding request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TaskDisposition {
    /// The request has enough edit or verification information to act.
    Edit,
    /// The request is a supported read-only question.
    Answer,
    /// The request needs a bounded clarification before any action.
    Clarify,
}

impl TaskDisposition {
    fn parse(value: &str) -> Result<Self, BlockBuildError> {
        match value {
            "edit" => Ok(Self::Edit),
            "answer" => Ok(Self::Answer),
            "clarify" => Ok(Self::Clarify),
            _ => Err(BlockBuildError::new(format!(
                "unknown task disposition '{value}'"
            ))),
        }
    }

    /// Returns this disposition's stable switch case label.
    #[must_use]
    pub const fn case_key(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Answer => "answer",
            Self::Clarify => "clarify",
        }
    }
}

/// Makes the accepted task available as a graph resource to the classification
/// and clarification blocks. A host that runs either block must insert the
/// same task whose workspace it is evaluating.
impl LocalResource for CodingTask {}

/// The closed set of dispositions a classifier instance may emit.
#[derive(Debug, Clone)]
struct AllowedDispositions(BTreeSet<TaskDisposition>);

impl AllowedDispositions {
    fn decode(config: &ResolvedConfigurationRef) -> Result<Self, BlockBuildError> {
        let entries = map(config)?;
        exact_keys(entries, &["allowed_dispositions"])?;
        let ConfigurationValue::List(values) = required(entries, "allowed_dispositions")? else {
            return Err(BlockBuildError::new("allowed_dispositions must be a list"));
        };
        let mut allowed = BTreeSet::new();
        for value in values {
            let ConfigurationValue::String(value) = value else {
                return Err(BlockBuildError::new(
                    "allowed_dispositions must contain only strings",
                ));
            };
            if !allowed.insert(TaskDisposition::parse(value)?) {
                return Err(BlockBuildError::new(
                    "allowed_dispositions contains a duplicate",
                ));
            }
        }
        if allowed.is_empty() {
            return Err(BlockBuildError::new(
                "allowed_dispositions must not be empty",
            ));
        }
        Ok(Self(allowed))
    }
}

/// The upper bound on questions a clarification step may ask.
#[derive(Debug, Clone, Copy)]
struct ClarificationLimit(usize);

impl ClarificationLimit {
    fn decode(config: &ResolvedConfigurationRef) -> Result<Self, BlockBuildError> {
        let entries = map(config)?;
        exact_keys(entries, &["max_questions"])?;
        let ConfigurationValue::Unsigned(value) = required(entries, "max_questions")? else {
            return Err(BlockBuildError::new(
                "max_questions must be an unsigned integer",
            ));
        };
        let value = usize::try_from(*value)
            .map_err(|_| BlockBuildError::new("max_questions does not fit in usize"))?;
        if value == 0 || value > MAX_QUESTIONS_LIMIT {
            return Err(BlockBuildError::new(format!(
                "max_questions must be in 1..={MAX_QUESTIONS_LIMIT}"
            )));
        }
        Ok(Self(value))
    }
}

fn classify(task: &CodingTask) -> TaskDisposition {
    if task.intent() == CodingIntent::Answer {
        return if task.question().is_answerable() {
            TaskDisposition::Answer
        } else {
            TaskDisposition::Clarify
        };
    }
    if !task.edits().is_empty() || matches!(task.success(), SuccessCheck::Command(_)) {
        TaskDisposition::Edit
    } else {
        TaskDisposition::Clarify
    }
}

#[system]
async fn classify_request(
    task: Res<CodingTask>,
    allowed: Out<AllowedDispositions>,
) -> Result<TaskDisposition, SystemError> {
    let disposition = classify(&task);
    if allowed.0.contains(&disposition) {
        Ok(disposition)
    } else {
        Err(exec(format!(
            "task disposition '{}' is not allowed by this block configuration",
            disposition.case_key()
        )))
    }
}

#[system]
async fn request_clarification(
    task: Res<CodingTask>,
    limit: Out<ClarificationLimit>,
) -> Result<AssessmentConclusion, SystemError> {
    let question = format!(
        "What concrete outcome should be produced for this request: {}?",
        task.statement().trim_end_matches('?')
    );
    let finding = QuestionFinding::new(
        task.statement().to_owned(),
        String::new(),
        0,
        format!(
            "Clarification requested (up to {} question{}): {question}",
            limit.0,
            if limit.0 == 1 { "" } else { "s" }
        ),
        Status::Indeterminate,
    );
    let assessment = finding_assessment(finding).map_err(exec)?;
    conclude_findings(vec![assessment], Vec::new(), StopReason::EscalatedOut).map_err(exec)
}

/// Classifies an accepted coding request into a configured closed set of
/// `edit`, `answer`, and `clarify` switch cases.
pub struct ClassifyRequestBlock;

impl BlockFactory for ClassifyRequestBlock {
    fn build(
        &self,
        graph: &mut Graph,
        config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        let allowed = AllowedDispositions::decode(config)?;
        graph.add_system_node(move || {
            let allowed = allowed.clone();
            async move { allowed }
        });
        Ok(graph.add_system_node(classify_request))
    }
}

/// Returns [`ClassifyRequestBlock`]'s registered key.
#[must_use]
pub fn classify_request_key() -> CompositionBlockKey {
    block_key("code.classify-request")
}

/// Returns [`ClassifyRequestBlock`]'s configuration schema key.
#[must_use]
pub fn classify_request_schema() -> ConfigurationSchemaKey {
    schema_key("polaris-code.gate-blocks.classify-request-config")
}

/// Concludes an underspecified request with one bounded clarification question.
pub struct RequestClarificationBlock;

impl BlockFactory for RequestClarificationBlock {
    fn build(
        &self,
        graph: &mut Graph,
        config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        let limit = ClarificationLimit::decode(config)?;
        graph.add_system_node(move || async move { limit });
        Ok(graph.add_system_node(request_clarification))
    }
}

/// Returns [`RequestClarificationBlock`]'s registered key.
#[must_use]
pub fn request_clarification_key() -> CompositionBlockKey {
    block_key("code.request-clarification")
}

/// Returns [`RequestClarificationBlock`]'s configuration schema key.
#[must_use]
pub fn request_clarification_schema() -> ConfigurationSchemaKey {
    schema_key("polaris-code.gate-blocks.request-clarification-config")
}

/// Returns the classifier discriminator's registered key.
#[must_use]
pub fn task_disposition_key() -> polaris_cosmos::composition::DiscriminatorKey {
    super::predicates::task_disposition_key()
}

/// Returns every block key this module registers.
#[must_use]
pub fn keys() -> Vec<CompositionBlockKey> {
    vec![classify_request_key(), request_clarification_key()]
}

/// Registers every clarification block in the sealed coding catalogue.
pub(super) fn register_all(
    catalogue: &mut CompositionCatalogue,
) -> Result<(), CompositionCatalogueError> {
    catalogue.register_block(
        classify_request_key(),
        Arc::new(ClassifyRequestBlock),
        classify_request_schema(),
        Effect::ReadOnly,
    )?;
    catalogue.describe_block(
        &classify_request_key(),
        describe(
            "Classifies an accepted coding request into a configured closed set of edit, \
             answer, and clarify dispositions.",
            Vec::new(),
            block_cards::task_disposition_port(),
            "first step of a plan that branches on request shape with a Switch keyed on the \
             classifier discriminator",
        ),
    )?;
    catalogue.register_block(
        request_clarification_key(),
        Arc::new(RequestClarificationBlock),
        request_clarification_schema(),
        Effect::ReadOnly,
    )?;
    catalogue.describe_block(
        &request_clarification_key(),
        describe(
            "Concludes an underspecified request with one bounded clarification question.",
            Vec::new(),
            block_cards::assessment_conclusion_port(),
            "the clarify arm of a Switch keyed on code.classify-request's disposition",
        ),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cosmos::task::SuccessCheck;

    fn config(
        schema: ConfigurationSchemaKey,
        entries: Vec<ConfigurationEntry>,
    ) -> ResolvedConfigurationRef {
        ResolvedConfigurationRef::inline(
            schema,
            ConfigurationValue::map(entries).expect("canonical configuration"),
        )
        .expect("resolved configuration")
    }

    #[test]
    fn classifier_distinguishes_actionable_and_underspecified_requests() {
        let dir = tempfile::tempdir().expect("temporary workspace");
        let ambiguous = CodingTask::from_statement(
            "ambiguous-request",
            dir.path(),
            "make it better",
            CodingIntent::Careful,
            SuccessCheck::stated_edits(),
        )
        .expect("task");
        assert_eq!(classify(&ambiguous), TaskDisposition::Clarify);

        let answer = CodingTask::from_statement(
            "answer-request",
            dir.path(),
            "where is the entry point",
            CodingIntent::Answer,
            SuccessCheck::stated_edits(),
        )
        .expect("task")
        .with_question("where is main");
        assert_eq!(classify(&answer), TaskDisposition::Answer);
    }

    #[test]
    fn strict_configurations_reject_unknown_fields_and_bad_bounds() {
        let unknown = config(
            classify_request_schema(),
            vec![ConfigurationEntry::new("other", ConfigurationValue::Null).expect("entry")],
        );
        assert!(AllowedDispositions::decode(&unknown).is_err());
        let zero = config(
            request_clarification_schema(),
            vec![
                ConfigurationEntry::new("max_questions", ConfigurationValue::Unsigned(0))
                    .expect("entry"),
            ],
        );
        assert!(ClarificationLimit::decode(&zero).is_err());
    }
}
