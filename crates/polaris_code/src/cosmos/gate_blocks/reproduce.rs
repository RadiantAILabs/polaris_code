//! Writing the reproduce script a statement-only repair loop runs before and
//! after its edit.
//!
//! A statement-only task states no test command (see
//! `docs/gate-planning.md`), so there is no [`ProjectTestRunner`] the task
//! itself named. [`WriteReproduceScriptBlock`] is the one new block this
//! shape needs: it takes the model's own [`ModelStepArtifact::Answer`] —
//! produced by an ordinary [`super::configurable::ModelProposeBlock`]
//! configured for [`ModelStepOutputKind::Answer`](super::configurable::ModelStepOutputKind::Answer),
//! asked to write a small script that reproduces the issue from the task's
//! statement — and writes it to [`REPRODUCE_SCRIPT_PATH`], a fixed,
//! plan-known location. Everything downstream of that write reuses existing
//! blocks unchanged: the host installs a [`ProjectTestRunner`] naming
//! [`REPRODUCE_SCRIPT_PROGRAM`] and [`REPRODUCE_SCRIPT_PATH`], so
//! [`super::repair::RunProjectTestsCapturedBlock`] and
//! [`super::repair::JudgeTestOutcomeBlock`] run and judge the script exactly
//! as they would a stated test command — once right after it is written, to
//! confirm it reproduces the issue, and again after the repair loop's own
//! edit, feeding a still-failing run back into that loop the same way a
//! stated test command's failure would.
//!
//! [`REPRODUCE_SCRIPT_PATH`] lives under [`crate::state::STATE_DIR_NAME`]
//! (`.polaris`), the agent's own state directory: [`crate::state::state_dir`]
//! keeps it self-ignored by git unconditionally, so the script never reaches
//! a `git add -A`/`git diff` patch — nothing here has to delete it before a
//! caller reads the final diff.

use std::fmt;

use polaris::graph::{Graph, NodeId};
use polaris::system::param::{Out, Res};
use polaris::system::system;
use polaris::system::system::SystemError;
use polaris_cosmos::composition::{
    BlockBuildError, BlockDescription, BlockFactory, CompositionCatalogue,
    CompositionCatalogueError, Effect,
};
use polaris_cosmos::contracts::{
    CompositionBlockId, CompositionBlockKey, ResolvedConfigurationRef,
};

use super::super::block_cards;
use super::super::payload::CodingWorkspace;
use super::behavior_version;
use super::configurable::ModelStepArtifact;

fn describe(
    summary: &str,
    inputs: Vec<polaris_cosmos::composition::StablePortKey>,
    output: polaris_cosmos::composition::StablePortKey,
    example: &str,
) -> BlockDescription {
    BlockDescription::new(summary, inputs, output, example)
        .expect("block description is well-formed")
}

fn exec(error: impl fmt::Display) -> SystemError {
    SystemError::ExecutionError(error.to_string())
}

fn block_key(id: &str) -> CompositionBlockKey {
    CompositionBlockKey::new(
        CompositionBlockId::new(id).expect("block ID is a valid stable name"),
        behavior_version(),
    )
}

/// Sandbox-relative path the reproduce script is always written to and run
/// from.
///
/// Fixed rather than carried as plan data: a gate plan names this exact
/// string, and the host installs a matching [`ProjectTestRunner`]
/// (`super::super::payload::ProjectTestRunner`) naming it, so
/// `code.run-project-tests-captured` can run the same script again after an
/// edit with no wiring between the write and the run at all — see the module
/// docs.
pub const REPRODUCE_SCRIPT_PATH: &str = ".polaris/reproduce_issue.py";

/// Leaf filename [`REPRODUCE_SCRIPT_PATH`] names under
/// [`crate::state::STATE_DIR_NAME`].
///
/// Kept separate from [`REPRODUCE_SCRIPT_PATH`] only so
/// [`write_reproduce_script`] can join it onto [`crate::state::state_dir`]'s
/// own return value rather than re-deriving `.polaris` by hand; `tests`
/// below checks the two constants agree.
const REPRODUCE_SCRIPT_FILENAME: &str = "reproduce_issue.py";

/// The interpreter the installed [`ProjectTestRunner`] runs
/// [`REPRODUCE_SCRIPT_PATH`] with.
///
/// Fixed, unlike the script's own content: this shape exists for a
/// statement-only task that states no test command itself but whose caller
/// grants a shell policy pattern like `python *`/`pytest *` (see
/// `docs/gate-planning.md`) — a Python interpreter is what those grants are
/// for. Generalising past Python — letting the model also choose how its
/// script is run — is out of scope for this block; `ProjectTestRunner`'s own
/// docs make the same choice not to name a language in its own identifier,
/// for the same reason this constant is confined to one small, documented
/// site rather than threaded through the block's own name.
pub const REPRODUCE_SCRIPT_PROGRAM: &str = "python";

/// What writing the reproduce script produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WroteReproduceScript {
    /// The sandbox-relative path the script was written to — always
    /// [`REPRODUCE_SCRIPT_PATH`], carried on the value so a reader does not
    /// have to import the constant to see what ran.
    pub path: String,
}

#[system]
async fn write_reproduce_script(
    workspace: Res<CodingWorkspace>,
    artifact: Out<ModelStepArtifact>,
) -> Result<WroteReproduceScript, SystemError> {
    let ModelStepArtifact::Answer(script) = &*artifact else {
        return Err(exec(
            "the reproduce-script step's model-propose block must be configured for an \
             answer artifact",
        ));
    };
    let state_dir = crate::state::state_dir(&workspace.config().working_dir).map_err(exec)?;
    std::fs::write(state_dir.join(REPRODUCE_SCRIPT_FILENAME), script).map_err(exec)?;
    Ok(WroteReproduceScript {
        path: REPRODUCE_SCRIPT_PATH.to_owned(),
    })
}

/// Writes the model's own [`ModelStepArtifact::Answer`] to
/// [`REPRODUCE_SCRIPT_PATH`], under the agent's self-ignored state directory.
///
/// Wraps [`write_reproduce_script`]: `CodingWorkspace, Out<ModelStepArtifact>
/// -> WroteReproduceScript`. Mutating and not repeatable in general — a
/// second run would overwrite the first's content with whatever the upstream
/// model step produced that time — though this plan never places it inside a
/// loop body, so repeat-safety never actually has to be relied on.
pub struct WriteReproduceScriptBlock;

impl BlockFactory for WriteReproduceScriptBlock {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(write_reproduce_script))
    }
}

/// Returns [`WriteReproduceScriptBlock`]'s registered key.
#[must_use]
pub fn write_reproduce_script_key() -> CompositionBlockKey {
    block_key("code.write-reproduce-script")
}

/// Every block key this module registers.
#[must_use]
pub fn keys() -> Vec<CompositionBlockKey> {
    vec![write_reproduce_script_key()]
}

/// Registers every block in this module into `catalogue`.
///
/// # Errors
///
/// Returns [`CompositionCatalogueError::DuplicateBlock`] if a key here
/// collides with one already registered.
pub(super) fn register_all(
    catalogue: &mut CompositionCatalogue,
) -> Result<(), CompositionCatalogueError> {
    catalogue.register_block(
        write_reproduce_script_key(),
        std::sync::Arc::new(WriteReproduceScriptBlock),
        super::no_config_schema(),
        Effect::Mutating { repeat_safe: false },
    )?;
    catalogue.describe_block(
        &write_reproduce_script_key(),
        describe(
            "Writes the model's own answer to a fixed, self-ignored path as the script a \
             statement-only repair loop runs to reproduce the issue.",
            vec![block_cards::model_step_artifact_port()],
            block_cards::wrote_reproduce_script_port(),
            "first step of a statement-only repair loop, wired after a code.model-propose \
             configured for an answer artifact and ahead of the loop's first \
             code.run-project-tests-captured",
        )
        .requires(block_cards::reproduce_runner_available()),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::any::TypeId;

    use polaris::graph::GraphExecutor;
    use polaris::system::param::{Access, SystemContext};

    use super::*;
    use polaris_cosmos::contracts::Status;

    use crate::cosmos::gate_blocks::configurable::{
        ModelProposeBlock, ModelStepArtifact, ModelStepConfig, ModelStepHandoff,
        ModelStepOutputKind, ModelStepProvider, ScriptedModelStep, TokenBudget, ToolSubset,
    };
    use crate::cosmos::task::{CodingIntent, CodingTask, SuccessCheck};
    use crate::usage::TokenMeter;

    fn no_config() -> ResolvedConfigurationRef {
        ResolvedConfigurationRef::inline(
            super::super::no_config_schema(),
            polaris_cosmos::contracts::ConfigurationValue::Null,
        )
        .expect("a null value satisfies the no-config schema")
    }

    fn guarantees(graph: &Graph, type_id: TypeId) -> bool {
        graph
            .signature()
            .guaranteed_produces()
            .iter()
            .any(|access: &Access| access.type_id == type_id)
    }

    fn workspace_with(dir: &std::path::Path) -> CodingWorkspace {
        std::fs::write(dir.join("lib.rs"), "todo!()").expect("seed file");
        let task = CodingTask::from_statement(
            "reproduce-block-fixture",
            dir,
            "fix the bug",
            CodingIntent::Careful,
            SuccessCheck::command("true", Vec::<String>::new()).expect("command check"),
        )
        .expect("well-formed task");
        CodingWorkspace::for_task(&task)
    }

    #[tokio::test]
    async fn write_reproduce_script_writes_the_models_answer_under_dot_polaris() {
        let dir = tempfile::tempdir().expect("temp project");
        let workspace = workspace_with(dir.path());
        let mut graph = Graph::new();
        ModelProposeBlock
            .build(
                &mut graph,
                &ModelStepConfig::new(
                    "reproduce".to_owned(),
                    "write a reproduce script".to_owned(),
                    ToolSubset::None,
                    TokenBudget(500),
                    ModelStepOutputKind::Answer,
                )
                .encode()
                .expect("well-formed configuration"),
            )
            .expect("builds");
        WriteReproduceScriptBlock
            .build(&mut graph, &no_config())
            .expect("builds");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
        assert!(guarantees(&graph, TypeId::of::<WroteReproduceScript>()));

        let mut context = SystemContext::new();
        context.insert(workspace);
        context.insert(TokenMeter::default());
        context.insert(ModelStepHandoff::default());
        context.insert(ModelStepProvider::new(ScriptedModelStep::new(
            ModelStepArtifact::Answer("print('reproducing')\n".to_owned()),
        )));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("propose then write runs");
        let wrote = execution
            .output::<WroteReproduceScript>()
            .expect("produces a WroteReproduceScript");
        assert_eq!(wrote.path, REPRODUCE_SCRIPT_PATH);

        let written = std::fs::read_to_string(dir.path().join(REPRODUCE_SCRIPT_PATH))
            .expect("script was written under .polaris");
        assert_eq!(written, "print('reproducing')\n");
    }

    #[tokio::test]
    async fn write_reproduce_script_refuses_a_non_answer_artifact() {
        // `PatchIntent` is excluded on purpose: a `ModelProposeBlock`
        // configured for it produces the concrete `PatchIntent` type, not
        // `ModelStepArtifact` at all (see `ModelProposeBlock::build`), so it
        // cannot even wire onto this block's `Out<ModelStepArtifact>` read.
        // `Review` is a real `ModelStepArtifact` variant this block must
        // still refuse, so it exercises the runtime check instead of a
        // graph-build-time wiring failure.
        let dir = tempfile::tempdir().expect("temp project");
        let workspace = workspace_with(dir.path());
        let mut graph = Graph::new();
        ModelProposeBlock
            .build(
                &mut graph,
                &ModelStepConfig::new(
                    "reproduce".to_owned(),
                    "write a reproduce script".to_owned(),
                    ToolSubset::None,
                    TokenBudget(500),
                    ModelStepOutputKind::Review,
                )
                .encode()
                .expect("well-formed configuration"),
            )
            .expect("builds");
        WriteReproduceScriptBlock
            .build(&mut graph, &no_config())
            .expect("builds");

        let mut context = SystemContext::new();
        context.insert(workspace);
        context.insert(TokenMeter::default());
        context.insert(ModelStepHandoff::default());
        context.insert(ModelStepProvider::new(ScriptedModelStep::new(
            ModelStepArtifact::Review {
                verdict: Status::Pass,
                notes: "not a script".to_owned(),
            },
        )));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await;
        assert!(
            execution.is_err(),
            "a review artifact must never be accepted as a script to write"
        );
    }

    #[test]
    fn reproduce_script_path_matches_the_state_dir_and_filename() {
        assert_eq!(
            REPRODUCE_SCRIPT_PATH,
            format!(
                "{}/{REPRODUCE_SCRIPT_FILENAME}",
                crate::state::STATE_DIR_NAME
            )
        );
    }

    #[test]
    fn every_key_is_distinct() {
        let mut keys = keys();
        let before = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), before);
    }
}
