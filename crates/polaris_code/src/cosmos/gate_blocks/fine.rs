//! Generic, bounded handoffs for the fine-grained statement-only plan.
//!
//! The `fine` arm starts each stage fresh; the `fine-ctx` arm continues the captured
//! conversation across stages. Both retain the same bounded typed answers and final patch
//! type consumed by the existing apply and verification blocks.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

use polaris::graph::{Graph, NodeId};
use polaris::system::param::{Out, Res};
use polaris::system::system;
use polaris::system::system::SystemError;
use polaris_cosmos::composition::{
    BlockBuildError, BlockFactory, CompositionCatalogue, CompositionCatalogueError, Effect,
};
use polaris_cosmos::contracts::{
    CompositionBlockId, CompositionBlockKey, ResolvedConfigurationRef,
};

use super::super::payload::CodingWorkspace;
use super::behavior_version;
use super::configurable::{
    ContextMode, ModelStepArtifact, ModelStepConfig, ModelStepHandoff, ModelStepOutputKind,
    ModelStepProvider, PatchIntent, ProposedPatchIntent, ToolSubset, WorkspaceView,
    run_with_handoff_conversation,
};
use crate::usage::TokenMeter;

const MAX_HANDOFF_CHARS: usize = 8_000;

#[derive(Clone)]
struct Located(String);

/// Bounded, structured observations passed from a cheap search step to a
/// fresh coding loop. Every suspect should name a path, symbol and line span.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocateFindings {
    /// Suspect paths, symbols and line spans with the observed reason.
    pub suspects: Vec<String>,
    /// Observed symptom and reproduction, if known.
    pub reproduction: String,
    /// Existing tests near the behavior, with file and line references.
    pub tests: Vec<String>,
    /// Unresolved questions for the following loop to check.
    pub open_questions: Vec<String>,
}

fn decode_locate_report(artifact: ModelStepArtifact) -> LocateFindings {
    let parsed = match artifact {
        ModelStepArtifact::Answer(text) if text.chars().count() <= 3_500 => {
            serde_json::from_str::<LocateFindings>(&text).ok()
        }
        _ => None,
    };
    parsed.unwrap_or_else(|| LocateFindings {
        suspects: Vec::new(),
        reproduction: "Location report unavailable; investigate from the issue statement"
            .to_owned(),
        tests: Vec::new(),
        open_questions: vec!["Which files and tests actually cover this issue?".to_owned()],
    })
}

#[system]
async fn cheap_locate(
    workspace: Res<CodingWorkspace>,
    config: Out<ModelStepConfig>,
    provider: Res<ModelStepProvider>,
    meter: Res<TokenMeter>,
    mut handoff: Res<ModelStepHandoff>,
) -> Result<LocateFindings, SystemError> {
    let view = WorkspaceView::new(&workspace);
    let (artifact, conversation) = match provider
        .propose_conversation(&config, &view, &meter, None)
        .await
    {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!(%error, "cheap location step failed; following loop will investigate independently");
            (ModelStepArtifact::Answer(String::new()), Default::default())
        }
    };
    let report = decode_locate_report(artifact);
    let report_text = serde_json::to_string(&report).map_err(|error| {
        SystemError::ExecutionError(format!("could not encode location report: {error}"))
    })?;
    handoff.record(
        &config,
        &ModelStepArtifact::Answer(report_text),
        conversation,
        0,
    );
    Ok(report)
}

#[derive(Clone)]
struct Reproduced(String);

fn bounded(answer: String) -> String {
    let mut chars = answer.chars();
    let short: String = chars.by_ref().take(MAX_HANDOFF_CHARS).collect();
    if chars.next().is_some() {
        format!("{short}\n[stage summary truncated]")
    } else {
        short
    }
}

fn answer(artifact: ModelStepArtifact) -> Result<String, SystemError> {
    match artifact {
        ModelStepArtifact::Answer(value) => Ok(bounded(value)),
        _ => Err(SystemError::ExecutionError(
            "fine stage expected an answer artifact".to_owned(),
        )),
    }
}

fn patch(artifact: ModelStepArtifact) -> Result<PatchIntent, SystemError> {
    match artifact {
        ModelStepArtifact::PatchIntent { edits, check } => Ok(PatchIntent { edits, check }),
        _ => Err(SystemError::ExecutionError(
            "fine stage expected a patch-intent artifact".to_owned(),
        )),
    }
}

#[system]
async fn locate(
    workspace: Res<CodingWorkspace>,
    config: Out<ModelStepConfig>,
    provider: Res<ModelStepProvider>,
    meter: Res<TokenMeter>,
    mut handoff: Res<ModelStepHandoff>,
) -> Result<Located, SystemError> {
    let view = WorkspaceView::new(&workspace);
    if config.context_mode != ContextMode::Fresh {
        return Ok(Located(answer(
            run_with_handoff_conversation(&config, &provider, &view, &meter, &handoff)
                .await?
                .0,
        )?));
    }
    Ok(Located(answer(
        provider.propose(&config, &view, &meter).await?,
    )?))
}

#[system]
async fn reproduce(
    workspace: Res<CodingWorkspace>,
    config: Out<ModelStepConfig>,
    located: Out<Located>,
    provider: Res<ModelStepProvider>,
    meter: Res<TokenMeter>,
    mut handoff: Res<ModelStepHandoff>,
) -> Result<Reproduced, SystemError> {
    let mut config = (*config).clone();
    config
        .instruction
        .push_str(&format!("\n\nLocate-stage findings:\n{}", located.0));
    let view = WorkspaceView::new(&workspace);
    if config.context_mode != ContextMode::Fresh {
        return Ok(Reproduced(answer(
            run_with_handoff_conversation(&config, &provider, &view, &meter, &handoff)
                .await?
                .0,
        )?));
    }
    Ok(Reproduced(answer(
        provider.propose(&config, &view, &meter).await?,
    )?))
}

fn with_handoffs(
    config: &ModelStepConfig,
    located: &Located,
    reproduced: &Reproduced,
) -> ModelStepConfig {
    let mut config = config.clone();
    config.instruction.push_str(&format!(
        "\n\nLocate-stage findings:\n{}\n\nReproduce-stage findings:\n{}",
        located.0, reproduced.0
    ));
    config
}

#[system]
async fn propose(
    workspace: Res<CodingWorkspace>,
    config: Out<ModelStepConfig>,
    located: Out<Located>,
    reproduced: Out<Reproduced>,
    provider: Res<ModelStepProvider>,
    meter: Res<TokenMeter>,
    mut handoff: Res<ModelStepHandoff>,
) -> Result<PatchIntent, SystemError> {
    let config = with_handoffs(&config, &located, &reproduced);
    let view = WorkspaceView::new(&workspace);
    if config.context_mode != ContextMode::Fresh {
        return patch(
            run_with_handoff_conversation(&config, &provider, &view, &meter, &handoff)
                .await?
                .0,
        );
    }
    patch(provider.propose(&config, &view, &meter).await?)
}

#[system]
async fn propose_conversation(
    workspace: Res<CodingWorkspace>,
    config: Out<ModelStepConfig>,
    located: Out<Located>,
    reproduced: Out<Reproduced>,
    provider: Res<ModelStepProvider>,
    meter: Res<TokenMeter>,
    mut handoff: Res<ModelStepHandoff>,
) -> Result<ProposedPatchIntent, SystemError> {
    let config = with_handoffs(&config, &located, &reproduced);
    let view = WorkspaceView::new(&workspace);
    let (artifact, conversation) = if config.context_mode == ContextMode::Fresh {
        provider
            .propose_conversation(&config, &view, &meter, None)
            .await?
    } else {
        run_with_handoff_conversation(&config, &provider, &view, &meter, &handoff).await?
    };
    Ok(ProposedPatchIntent {
        patch: patch(artifact)?,
        conversation,
    })
}

/// A generic model stage in the fine-grained proposal pipeline.
pub struct FineStageBlock {
    stage: Stage,
}

#[derive(Clone, Copy)]
enum Stage {
    Locate,
    Reproduce,
    Propose,
    ProposeConversation,
}

impl BlockFactory for FineStageBlock {
    fn build(
        &self,
        graph: &mut Graph,
        config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        let decoded = ModelStepConfig::decode(config)?;
        if matches!(&decoded.tool_subset, ToolSubset::Only(tools) if tools.iter().any(|tool| tool == "edit_file"))
        {
            return Err(BlockBuildError::new(
                "fine stage is read-only and cannot offer edit_file",
            ));
        }
        if matches!(self.stage, Stage::Locate)
            && decoded.role == "cheap-locate"
            && !matches!(&decoded.tool_subset, ToolSubset::Only(tools) if tools.iter().all(|tool| matches!(tool.as_str(), "read_file" | "list_dir" | "search")))
        {
            return Err(BlockBuildError::new(
                "cheap locate permits only read_file, list_dir and search",
            ));
        }
        let expected = match self.stage {
            Stage::Locate | Stage::Reproduce => ModelStepOutputKind::Answer,
            Stage::Propose | Stage::ProposeConversation => ModelStepOutputKind::PatchIntent,
        };
        if decoded.output != expected {
            return Err(BlockBuildError::new("fine stage has the wrong output kind"));
        }
        let is_cheap_locate = matches!(self.stage, Stage::Locate) && decoded.role == "cheap-locate";
        graph.add_system_node(move || {
            let decoded = decoded.clone();
            async move { decoded }
        });
        Ok(match self.stage {
            Stage::Locate if is_cheap_locate => graph.add_system_node(cheap_locate),
            Stage::Locate => graph.add_system_node(locate),
            Stage::Reproduce => graph.add_system_node(reproduce),
            Stage::Propose => graph.add_system_node(propose),
            Stage::ProposeConversation => graph.add_system_node(propose_conversation),
        })
    }
}

fn key(id: &str) -> CompositionBlockKey {
    CompositionBlockKey::new(
        CompositionBlockId::new(id).expect("fine block ID is valid"),
        behavior_version(),
    )
}

/// Key for the location stage.
pub fn locate_key() -> CompositionBlockKey {
    key("code.fine-locate")
}
/// Key for the reproduction stage.
pub fn reproduce_key() -> CompositionBlockKey {
    key("code.fine-reproduce")
}
/// Key for the patch proposal stage.
pub fn propose_key() -> CompositionBlockKey {
    key("code.fine-propose")
}
/// Key for the patch proposal stage that retains its conversation for repair.
pub fn propose_conversation_key() -> CompositionBlockKey {
    key("code.fine-propose-conversation")
}

/// Every fine-stage block key.
pub fn keys() -> Vec<CompositionBlockKey> {
    vec![
        locate_key(),
        reproduce_key(),
        propose_key(),
        propose_conversation_key(),
    ]
}

/// Registers the fine-stage blocks in the coding catalogue.
pub fn register_all(catalogue: &mut CompositionCatalogue) -> Result<(), CompositionCatalogueError> {
    for (key, stage) in [
        (locate_key(), Stage::Locate),
        (reproduce_key(), Stage::Reproduce),
        (propose_key(), Stage::Propose),
        (propose_conversation_key(), Stage::ProposeConversation),
    ] {
        catalogue.register_block(
            key,
            Arc::new(FineStageBlock { stage }),
            super::configurable::model_propose_schema(),
            Effect::ReadOnly,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;

    use polaris::graph::GraphExecutor;
    use polaris::system::param::SystemContext;

    use super::*;
    use crate::cosmos::gate_blocks::configurable::{ModelStepExecutor, TokenBudget};
    use crate::cosmos::task::{CodingIntent, CodingTask, ExactEdit};

    #[test]
    fn malformed_location_report_falls_back_to_independent_investigation() {
        let report = decode_locate_report(ModelStepArtifact::Answer("not JSON".to_owned()));
        assert!(report.suspects.is_empty());
        assert!(report.reproduction.contains("unavailable"));
        assert!(!report.open_questions.is_empty());
    }

    struct StageExecutor(Arc<Mutex<Vec<(String, String)>>>);

    impl ModelStepExecutor for StageExecutor {
        fn propose<'a>(
            &'a self,
            config: &'a ModelStepConfig,
            _workspace: &'a WorkspaceView<'a>,
            _meter: &'a TokenMeter,
        ) -> Pin<Box<dyn Future<Output = Result<ModelStepArtifact, SystemError>> + Send + 'a>>
        {
            Box::pin(async move {
                self.0
                    .lock()
                    .expect("record lock")
                    .push((config.role.clone(), config.instruction.clone()));
                match config.role.as_str() {
                    "locate" => Ok(ModelStepArtifact::Answer(
                        "lib.rs contains the broken operation".to_owned(),
                    )),
                    "reproduce" => Ok(ModelStepArtifact::Answer(
                        "the focused check fails before the patch".to_owned(),
                    )),
                    "propose" => Ok(ModelStepArtifact::PatchIntent {
                        edits: vec![ExactEdit::new("lib.rs", "todo!()", "1 + 1").expect("edit")],
                        check: None,
                    }),
                    other => panic!("unexpected stage: {other}"),
                }
            })
        }
    }

    #[tokio::test]
    async fn stage_outputs_cross_the_boundaries_in_order() {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::write(dir.path().join("lib.rs"), "todo!()").expect("source");
        let task = CodingTask::new(
            "fine-test",
            dir.path(),
            "lib.rs",
            "todo!()",
            "1 + 1",
            "todo!()",
            CodingIntent::Careful,
        )
        .expect("task");
        let mut graph = Graph::new();
        for (stage, role, output) in [
            (Stage::Locate, "locate", ModelStepOutputKind::Answer),
            (Stage::Reproduce, "reproduce", ModelStepOutputKind::Answer),
            (Stage::Propose, "propose", ModelStepOutputKind::PatchIntent),
        ] {
            let config = ModelStepConfig::new(
                role.to_owned(),
                "base instruction".to_owned(),
                ToolSubset::None,
                TokenBudget(100),
                output,
            )
            .encode()
            .expect("config");
            FineStageBlock { stage }
                .build(&mut graph, &config)
                .expect("stage builds");
        }
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut context = SystemContext::new();
        context.insert(CodingWorkspace::for_task(&task));
        context.insert(TokenMeter::default());
        context.insert(ModelStepHandoff::default());
        context.insert(ModelStepProvider::new(StageExecutor(Arc::clone(&calls))));
        let result = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("stages execute");
        assert_eq!(
            result
                .output::<PatchIntent>()
                .expect("patch output")
                .edits
                .len(),
            1
        );
        let calls = calls.lock().expect("record lock");
        assert_eq!(
            calls
                .iter()
                .map(|(role, _)| role.as_str())
                .collect::<Vec<_>>(),
            ["locate", "reproduce", "propose"]
        );
        assert!(calls[1].1.contains("lib.rs contains the broken operation"));
        assert!(calls[2].1.contains("lib.rs contains the broken operation"));
        assert!(
            calls[2]
                .1
                .contains("the focused check fails before the patch")
        );
    }
}
