//! Tests for typed, schedule-bearing system erasure
//! (`polaris::system::system::erase`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use polaris::graph::hooks::HooksAPI;
use polaris::graph::hooks::events::GraphEvent;
use polaris::graph::{Graph, GraphExecutor};
use polaris::system::param::{Out, ResMut, SystemContext};
use polaris::system::resource::{LocalResource, Output};
use polaris::system::system;
use polaris::system::system::{TypedSystem, erase};
use polaris_cosmos::cognition::prediction_error_loop::{Policies, Stages, build as build_direct};
use polaris_cosmos::cognition::{OnAct, OnPredict, OnPredictionError, OnVerify};

const BASELINE_CALLER_VISIBLE_TYPE_PARAMETERS: usize = 18;
const BASELINE_DOMAIN_CONTRACT_TYPE_PARAMETERS: usize = 4;
const ERASED_CALLER_VISIBLE_TYPE_PARAMETERS: usize = 4;
const ERASED_SYSTEM_TYPE_PARAMETERS: usize = 0;
const ERASED_INTO_SYSTEM_MARKER_PARAMETERS: usize = 0;
const COGNITION_MODEL_TYPE_PARAMETER_CEILING: usize = 4;
const EPISODES: u32 = 4;

#[derive(Clone)]
struct Prediction(u32);

#[derive(Clone)]
struct ActOutput(u32);

#[derive(Clone)]
struct Verified(u32);

#[derive(Clone)]
struct Feedback {
    episode: u32,
    error: i32,
}

#[derive(Debug, PartialEq, Eq)]
struct RunState {
    episodes: u32,
    accepted: u32,
    rejected: u32,
}
impl LocalResource for RunState {}

async fn initialize() -> Feedback {
    Feedback {
        episode: 0,
        error: 0,
    }
}

#[system]
async fn predict(feedback: Out<Feedback>) -> Prediction {
    Prediction(feedback.episode + 1)
}

#[system]
async fn act(prediction: Out<Prediction>) -> ActOutput {
    ActOutput(prediction.0 * 2)
}

#[system]
async fn verify(action: Out<ActOutput>) -> Verified {
    Verified(action.0 / 2)
}

#[system]
async fn compare(
    prediction: Out<Prediction>,
    verified: Out<Verified>,
    mut state: ResMut<RunState>,
) -> Feedback {
    assert_eq!(prediction.0, verified.0);
    state.episodes = prediction.0;
    Feedback {
        episode: prediction.0,
        error: if prediction.0.is_multiple_of(2) {
            1
        } else {
            -1
        },
    }
}

#[system]
async fn accept(mut state: ResMut<RunState>) {
    state.accepted += 1;
}

#[system]
async fn reject(mut state: ResMut<RunState>) {
    state.rejected += 1;
}

struct ErasedStages<Feedback, Prediction, ActOutput, Verified>
where
    Feedback: Output,
    Prediction: Output,
    ActOutput: Output,
    Verified: Output,
{
    initialize: TypedSystem<Feedback>,
    predict: TypedSystem<Prediction>,
    act: TypedSystem<ActOutput>,
    verify: TypedSystem<Verified>,
    compare: TypedSystem<Feedback>,
}

type FeedbackPredicate<Feedback> = Box<dyn Fn(&Feedback) -> bool + Send + Sync + 'static>;
type BranchBuilder = Box<dyn FnOnce(&mut Graph) + 'static>;

struct ErasedPolicies<Feedback> {
    terminate: FeedbackPredicate<Feedback>,
    gate: FeedbackPredicate<Feedback>,
    accept: BranchBuilder,
    reject: BranchBuilder,
}

fn build_erased<'g, Feedback, Prediction, ActOutput, Verified>(
    graph: &'g mut Graph,
    loop_name: &'static str,
    stages: ErasedStages<Feedback, Prediction, ActOutput, Verified>,
    policies: ErasedPolicies<Feedback>,
) -> &'g mut Graph
where
    Feedback: Output,
    Prediction: Output,
    ActOutput: Output,
    Verified: Output,
{
    let ErasedStages {
        initialize,
        predict,
        act,
        verify,
        compare,
    } = stages;
    let ErasedPolicies {
        terminate,
        gate,
        accept,
        reject,
    } = policies;

    graph.system_boxed(initialize).done();
    graph.add_loop::<Feedback, _, _>(loop_name, terminate, move |body| {
        body.system_boxed(predict).with_schedules(OnPredict).done();
        body.system_boxed(act).with_schedules(OnAct).done();
        body.system_boxed(verify).with_schedules(OnVerify).done();
        body.system_boxed(compare)
            .with_schedules(OnPredictionError)
            .done();
        body.add_conditional_branch::<Feedback, _, _, _>(
            "prediction_error_gate",
            gate,
            accept,
            reject,
        );
    });
    graph
}

fn direct_graph() -> Graph {
    let mut graph = Graph::new();
    build_direct(
        &mut graph,
        "prediction_error_loop",
        Stages::new(initialize, predict, act, verify, compare),
        Policies::new(
            |feedback: &Feedback| feedback.episode >= EPISODES,
            |feedback: &Feedback| feedback.error >= 0,
            |branch: &mut Graph| {
                branch.add_system(accept);
            },
            |branch: &mut Graph| {
                branch.add_system(reject);
            },
        ),
    );
    graph
}

fn erased_graph() -> Graph {
    let mut graph = Graph::new();
    build_erased(
        &mut graph,
        "prediction_error_loop",
        ErasedStages {
            initialize: erase(initialize),
            predict: erase(predict),
            act: erase(act),
            verify: erase(verify),
            compare: erase(compare),
        },
        ErasedPolicies {
            terminate: Box::new(|feedback: &Feedback| feedback.episode >= EPISODES),
            gate: Box::new(|feedback: &Feedback| feedback.error >= 0),
            accept: Box::new(|branch: &mut Graph| {
                branch.add_system(accept);
            }),
            reject: Box::new(|branch: &mut Graph| {
                branch.add_system(reject);
            }),
        },
    );
    graph
}

#[derive(Debug, PartialEq, Eq)]
struct Observation {
    role_emissions: [usize; 4],
    episodes: u32,
    accepted: u32,
    rejected: u32,
    final_episode: u32,
}

async fn execute(graph: &Graph) -> Observation {
    let hooks = HooksAPI::new();
    let predicts = Arc::new(AtomicUsize::new(0));
    let acts = Arc::new(AtomicUsize::new(0));
    let verifies = Arc::new(AtomicUsize::new(0));
    let errors = Arc::new(AtomicUsize::new(0));

    macro_rules! count_starts {
        ($schedule:ty, $counter:expr) => {{
            let counter = Arc::clone(&$counter);
            hooks
                .register_observer::<$schedule, _>(
                    concat!("count_", stringify!($schedule)),
                    move |event| {
                        if matches!(event, GraphEvent::SystemStart { .. }) {
                            counter.fetch_add(1, Ordering::SeqCst);
                        }
                    },
                )
                .expect("hook registration should succeed");
        }};
    }

    count_starts!(OnPredict, predicts);
    count_starts!(OnAct, acts);
    count_starts!(OnVerify, verifies);
    count_starts!(OnPredictionError, errors);

    let mut context = SystemContext::new().with(RunState {
        episodes: 0,
        accepted: 0,
        rejected: 0,
    });
    let executor = GraphExecutor::new();
    executor
        .validate_resources(graph, &context, Some(&hooks))
        .expect("consumer graph resources should validate");
    executor
        .execute(graph, &mut context, Some(&hooks), None)
        .await
        .expect("consumer graph should execute");

    let state = context
        .get_resource::<RunState>()
        .expect("run state should remain available");
    let episodes = state.episodes;
    let accepted = state.accepted;
    let rejected = state.rejected;
    drop(state);
    let final_episode = context
        .get_output::<Feedback>()
        .expect("comparison should leave final feedback")
        .episode;

    Observation {
        role_emissions: [
            predicts.load(Ordering::SeqCst),
            acts.load(Ordering::SeqCst),
            verifies.load(Ordering::SeqCst),
            errors.load(Ordering::SeqCst),
        ],
        episodes,
        accepted,
        rejected,
        final_episode,
    }
}

#[test]
fn erased_helper_meets_the_cognition_model_generic_budget() {
    assert_eq!(BASELINE_CALLER_VISIBLE_TYPE_PARAMETERS, 18);
    assert_eq!(BASELINE_DOMAIN_CONTRACT_TYPE_PARAMETERS, 4);
    assert_eq!(ERASED_SYSTEM_TYPE_PARAMETERS, 0);
    assert_eq!(ERASED_INTO_SYSTEM_MARKER_PARAMETERS, 0);
    assert_eq!(
        ERASED_CALLER_VISIBLE_TYPE_PARAMETERS,
        COGNITION_MODEL_TYPE_PARAMETER_CEILING,
    );
    assert_eq!(
        ERASED_CALLER_VISIBLE_TYPE_PARAMETERS, BASELINE_DOMAIN_CONTRACT_TYPE_PARAMETERS,
        "the erased helper exposes only the four domain contract types",
    );

    let _monomorphized_builder = build_erased::<Feedback, Prediction, ActOutput, Verified>;
}

#[tokio::test]
async fn erased_path_preserves_signature_behavior_and_role_emissions() {
    let direct = direct_graph();
    let erased = erased_graph();

    assert!(direct.validate().is_ok(), "direct graph should validate");
    assert!(erased.validate().is_ok(), "erased graph should validate");
    assert_eq!(
        direct.signature(),
        erased.signature(),
        "typed erasure must preserve the native graph signature",
    );

    let direct_observation = execute(&direct).await;
    let erased_observation = execute(&erased).await;
    assert_eq!(direct_observation, erased_observation);
    assert_eq!(
        erased_observation.role_emissions, [EPISODES as usize; 4],
        "each erased stage must emit its Cosmos role once per episode",
    );
}

struct TemporaryCrate {
    root: PathBuf,
}

impl TemporaryCrate {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "polaris_cosmos_{name}_{}_{}",
            std::process::id(),
            nonce,
        ));
        fs::create_dir_all(root.join("src")).expect("temporary crate should be created");
        Self { root }
    }

    fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for TemporaryCrate {
    fn drop(&mut self) {
        let _cleanup_result = fs::remove_dir_all(&self.root);
    }
}

/// Renders the workspace's `polaris` dependency as a `polaris-ai` dependency line,
/// so the scratch crate builds against the same Polaris source and version.
fn workspace_polaris_dependency(workspace_manifest: &str) -> String {
    let entry = workspace_manifest
        .lines()
        .find_map(|line| line.trim().strip_prefix("polaris = "))
        .expect("workspace manifest should declare a `polaris` dependency");
    format!(
        "polaris-ai = {}",
        entry.replacen("package = \"polaris-ai\", ", "", 1)
    )
}

#[test]
fn mismatched_stage_output_is_rejected_at_compile_time() {
    let fixture = TemporaryCrate::new("typed_erasure_compile_fail");
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate should live at <workspace>/crates/<name>");
    let workspace_manifest = fs::read_to_string(workspace_root.join("Cargo.toml"))
        .expect("workspace manifest should be readable");
    let polaris_dependency = workspace_polaris_dependency(&workspace_manifest);

    let manifest = format!(
        "[package]\nname = \"typed-erasure-compile-fail\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\n{polaris_dependency}\n",
    );
    fs::write(fixture.root().join("Cargo.toml"), manifest)
        .expect("temporary manifest should be written");
    // The check runs offline, so pin it to the versions the workspace already fetched.
    fs::copy(
        workspace_root.join("Cargo.lock"),
        fixture.root().join("Cargo.lock"),
    )
    .expect("workspace lockfile should be copied");
    fs::write(
        fixture.root().join("src/main.rs"),
        r#"
use polaris_ai::system::system::{TypedSystem, erase};

struct Prediction;
struct WrongStageOutput;

async fn wrong_stage() -> WrongStageOutput {
    WrongStageOutput
}

fn prediction_slot(_stage: TypedSystem<Prediction>) {}

fn main() {
    prediction_slot(erase(wrong_stage));
}
"#,
    )
    .expect("temporary source should be written");

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let result = Command::new(cargo)
        .args(["check", "--offline", "--quiet", "--color", "never"])
        .current_dir(fixture.root())
        .env_remove("RUSTC_WRAPPER")
        .env("CARGO_TARGET_DIR", fixture.root().join("target"))
        .output()
        .expect("temporary compile-fail check should run");
    let stderr = String::from_utf8_lossy(&result.stderr);

    assert!(
        !result.status.success(),
        "a wrong stage output unexpectedly compiled",
    );
    assert!(
        stderr.contains("mismatched types")
            && stderr.contains("TypedSystem<Prediction>")
            && stderr.contains("TypedSystem<WrongStageOutput>"),
        "compile failure should be the typed stage mismatch, stderr:\n{stderr}",
    );
}
