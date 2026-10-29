//! A prediction-error cognitive model.
//!
//! This model repeats one domain-neutral episode:
//!
//! ```text
//! predict → act → verify → compare → gate ─┬─ accept
//!                                          └─ reject
//! ```
//!
//! Agent builders supply every system and payload type. Cosmos owns only the
//! logical topology and its semantic role schedules. The resulting graph's
//! [`GraphSignature`] is the model's structural contract; no prediction,
//! action, verifier, error, or memory representation is canonical.
//!
//! [`build`] remains usable without topology discovery. When an application
//! does use the catalog, [`topology_key`] supplies the stable model identity,
//! [`descriptor`] combines the model metadata with an embodiment contract, and
//! [`contract_signature`] derives that contract's structural portion for the
//! concrete payload types.

use polaris::graph::{Graph, GraphSignature};
use polaris::system::plugin::Version;
use polaris::system::resource::Output;
use polaris::system::system::{IntoSystem, System};

use super::{OnAct, OnPredict, OnPredictionError, OnVerify};
use crate::topology::{TopologyContractKey, TopologyDescriptor, TopologyId, TopologyKey};

/// Stable catalog identity of the prediction-error-loop topology definition.
pub const TOPOLOGY_ID: &str = "cosmos.prediction-error-loop";

/// Behaviour version of the prediction-error-loop topology definition.
pub const TOPOLOGY_VERSION: Version = Version::new(0, 1, 0);

/// Returns the stable versioned identity of this topology definition.
#[must_use]
pub fn topology_key() -> TopologyKey {
    TopologyKey::new(
        TopologyId::new(TOPOLOGY_ID).expect("prediction-error-loop topology ID is valid"),
        TOPOLOGY_VERSION,
    )
}

/// Creates this model's catalog descriptor for an embodiment contract.
///
/// The supplied contract identifies the concrete task/result semantics and
/// payload types chosen by an embodiment. Callers may append further claims
/// with [`TopologyDescriptor::claim`] before registration.
#[must_use]
pub fn descriptor(contract: TopologyContractKey) -> TopologyDescriptor {
    TopologyDescriptor::new(
        topology_key(),
        "Prediction-error loop",
        "Predict, act, verify, compare the observation, and gate the update",
    )
    .expect("prediction-error-loop descriptor metadata is valid")
    .claim(contract)
}

/// Systems occupying the five stages of a [`build`] invocation.
pub struct Stages<Initialize, Predict, Act, Verify, Compare> {
    initialize: Initialize,
    predict: Predict,
    act: Act,
    verify: Verify,
    compare: Compare,
}

impl<Initialize, Predict, Act, Verify, Compare> Stages<Initialize, Predict, Act, Verify, Compare> {
    /// Creates a set of stage systems in execution order.
    pub fn new(
        initialize: Initialize,
        predict: Predict,
        act: Act,
        verify: Verify,
        compare: Compare,
    ) -> Self {
        Self {
            initialize,
            predict,
            act,
            verify,
            compare,
        }
    }
}

/// Control policies and branch builders for a [`build`] invocation.
pub struct Policies<Terminate, Gate, Accept, Reject> {
    terminate: Terminate,
    gate: Gate,
    accept: Accept,
    reject: Reject,
}

impl<Terminate, Gate, Accept, Reject> Policies<Terminate, Gate, Accept, Reject> {
    /// Creates loop termination, update-gate, and branch policies.
    pub fn new(terminate: Terminate, gate: Gate, accept: Accept, reject: Reject) -> Self {
        Self {
            terminate,
            gate,
            accept,
            reject,
        }
    }
}

/// Returns the catalog-contract signature for a prediction-error loop using
/// the supplied embodiment payload types.
///
/// Polaris v1 conservatively treats `Out<T>` reads inside a loop body as free
/// reads even when another body system produces `T` earlier in the iteration.
/// The three stage handoffs therefore appear on `requires_outputs`; the final
/// `Feedback` is the capability's promised output. Required resources and
/// additional outputs remain embodiment-specific and are admitted by
/// [`GraphSignature::satisfies`].
///
/// The semantic contract ID is deliberately supplied by the embodiment rather
/// than fixed here: coding, research, and robotics registrations share one
/// [`TOPOLOGY_ID`] while exposing different typed task/result ports.
#[must_use]
pub fn contract_signature<Prediction, ActOutput, Verified, Feedback>() -> GraphSignature
where
    Prediction: Output,
    ActOutput: Output,
    Verified: Output,
    Feedback: Output,
{
    GraphSignature::new()
        .require_output::<Prediction>()
        .require_output::<ActOutput>()
        .require_output::<Verified>()
        .produce::<Feedback>()
}

/// Builds the prediction-error-loop cognitive model into `graph`.
///
/// `initialize` and `compare` both produce `Feedback`. Initialization makes a
/// value available for Polaris's pre-iteration termination check; each compare
/// then replaces it for the gate and the next termination check. `predict`,
/// `act`, `verify`, and `compare` are tagged with the shared role schedules.
///
/// Dependencies between stages remain ordinary typed Polaris data flow. For
/// example, an embodiment's verify system may read `Out<ActOutput>`, and its
/// compare system may read `Out<Prediction>` and `Out<Verified>`.
///
/// The accept and reject closures build policy-specific branch graphs. They
/// may consolidate memory, record a veto, update credit, or produce a rewire
/// proposal; this model fixes none of those representations.
pub fn build<
    'g,
    Feedback,
    Prediction,
    ActOutput,
    Verified,
    Initialize,
    MInitialize,
    Predict,
    MPredict,
    Act,
    MAct,
    Verify,
    MVerify,
    Compare,
    MCompare,
    Terminate,
    Gate,
    Accept,
    Reject,
>(
    graph: &'g mut Graph,
    loop_name: &'static str,
    stages: Stages<Initialize, Predict, Act, Verify, Compare>,
    policies: Policies<Terminate, Gate, Accept, Reject>,
) -> &'g mut Graph
where
    Feedback: Output,
    Prediction: Output,
    ActOutput: Output,
    Verified: Output,
    Initialize: IntoSystem<MInitialize>,
    Initialize::System: System<Output = Feedback>,
    Predict: IntoSystem<MPredict>,
    Predict::System: System<Output = Prediction>,
    Act: IntoSystem<MAct>,
    Act::System: System<Output = ActOutput>,
    Verify: IntoSystem<MVerify>,
    Verify::System: System<Output = Verified>,
    Compare: IntoSystem<MCompare>,
    Compare::System: System<Output = Feedback>,
    Terminate: Fn(&Feedback) -> bool + Send + Sync + 'static,
    Gate: Fn(&Feedback) -> bool + Send + Sync + 'static,
    Accept: FnOnce(&mut Graph),
    Reject: FnOnce(&mut Graph),
{
    let Stages {
        initialize,
        predict,
        act,
        verify,
        compare,
    } = stages;
    let Policies {
        terminate,
        gate,
        accept,
        reject,
    } = policies;

    graph.add_system(initialize);
    graph.add_loop::<Feedback, _, _>(loop_name, terminate, move |body| {
        body.add_system((OnPredict, predict));
        body.add_system((OnAct, act));
        body.add_system((OnVerify, verify));
        body.add_system((OnPredictionError, compare));
        body.add_conditional_branch::<Feedback, _, _, _>(
            "prediction_error_gate",
            gate,
            accept,
            reject,
        );
    });
    graph
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use polaris::graph::GraphExecutor;
    use polaris::graph::hooks::HooksAPI;
    use polaris::graph::hooks::events::GraphEvent;
    use polaris::system::param::{Out, Res, ResMut, SystemContext};
    use polaris::system::resource::LocalResource;
    use polaris::system::system;

    use super::*;

    #[derive(Clone)]
    struct Prediction(f64);

    #[derive(Clone)]
    struct ActOutput(u32);

    #[derive(Clone)]
    struct Verified(f64);

    #[derive(Clone)]
    struct Feedback {
        error: f64,
        episode: u32,
    }

    struct RunState {
        episodes: u32,
        accepted: u32,
        rejected: u32,
    }
    impl LocalResource for RunState {}

    #[system]
    async fn initialize() -> Feedback {
        Feedback {
            error: 0.0,
            episode: 0,
        }
    }

    #[system]
    async fn predict() -> Prediction {
        Prediction(0.5)
    }

    #[system]
    async fn act(mut state: ResMut<RunState>) -> ActOutput {
        state.episodes += 1;
        ActOutput(state.episodes)
    }

    #[system]
    async fn verify(action: Out<ActOutput>) -> Verified {
        if action.0 % 2 == 1 {
            Verified(1.0)
        } else {
            Verified(0.0)
        }
    }

    #[system]
    async fn compare(
        prediction: Out<Prediction>,
        verified: Out<Verified>,
        state: Res<RunState>,
    ) -> Feedback {
        Feedback {
            error: verified.0 - prediction.0,
            episode: state.episodes,
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

    #[tokio::test]
    async fn executes_both_gate_paths_and_emits_each_role_per_episode() {
        const EPISODES: u32 = 4;

        let mut graph = Graph::new();
        build(
            &mut graph,
            "prediction_error_loop",
            Stages::new(initialize, predict, act, verify, compare),
            Policies::new(
                |feedback: &Feedback| feedback.episode >= EPISODES,
                |feedback: &Feedback| feedback.error >= 0.0,
                |branch: &mut Graph| {
                    branch.add_system(accept);
                },
                |branch: &mut Graph| {
                    branch.add_system(reject);
                },
            ),
        );

        let validation = graph.validate();
        assert!(
            validation.is_ok(),
            "model graph should validate: {:?}",
            validation.errors()
        );

        let contract = contract_signature::<Prediction, ActOutput, Verified, Feedback>();
        let signature = graph.signature();
        let diff = signature.satisfies_diff(&contract);
        assert!(diff.is_empty(), "model capability drifted: {diff}");

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

        let mut ctx = SystemContext::new().with(RunState {
            episodes: 0,
            accepted: 0,
            rejected: 0,
        });
        let executor = GraphExecutor::new();
        executor
            .validate_resources(&graph, &ctx, Some(&hooks))
            .expect("model resources should validate");
        executor
            .execute(&graph, &mut ctx, Some(&hooks), None)
            .await
            .expect("model should execute");

        let state = ctx
            .get_resource::<RunState>()
            .expect("run state should remain available");
        assert_eq!(state.episodes, EPISODES);
        assert_eq!(state.accepted, 2);
        assert_eq!(state.rejected, 2);
        drop(state);

        let feedback = ctx
            .get_output::<Feedback>()
            .expect("comparison should leave final feedback");
        assert_eq!(feedback.episode, EPISODES);

        let expected = EPISODES as usize;
        assert_eq!(predicts.load(Ordering::SeqCst), expected);
        assert_eq!(acts.load(Ordering::SeqCst), expected);
        assert_eq!(verifies.load(Ordering::SeqCst), expected);
        assert_eq!(errors.load(Ordering::SeqCst), expected);
    }
}
