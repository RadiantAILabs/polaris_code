//! Turn queue and safe-boundary steering contract tests.

use polaris_code::terminal::TurnSteering;
use polaris_tui::panels::conversation::{TurnInputHandle, TurnInputMode};

#[tokio::test]
async fn queue_targets_future_runs_and_remains_editable_until_consumed() {
    let steering = TurnSteering::enabled();
    assert_eq!(steering.activate(), 1);

    let first = steering
        .submit("first".into(), TurnInputMode::Queue)
        .unwrap();
    let second = steering
        .submit("second".into(), TurnInputMode::Queue)
        .unwrap();
    assert_eq!((first.target_run, second.target_run), (1, 2));

    steering.replace(first.id, "edited first".into()).unwrap();
    steering.replace(second.id, "edited second".into()).unwrap();
    assert_eq!(steering.receive_prompt().await, "edited first");
    steering.finish();

    assert_eq!(steering.activate(), 2);
    assert_eq!(steering.receive_prompt().await, "edited second");
}

#[tokio::test]
async fn steering_waits_for_a_safe_boundary_and_survives_early_turn_end() {
    let steering = TurnSteering::enabled();
    steering.activate();
    steering
        .submit("initial task".into(), TurnInputMode::Queue)
        .unwrap();
    assert_eq!(steering.receive_prompt().await, "initial task");
    let applied = steering
        .submit("change direction".into(), TurnInputMode::Steer)
        .unwrap();
    assert_eq!(applied.target_run, 1);
    assert_eq!(steering.take_steering(), ["change direction"]);

    let retained = steering
        .submit("do not lose this".into(), TurnInputMode::Steer)
        .unwrap();
    assert_eq!(retained.target_run, 1);
    steering.finish();

    steering.activate();
    assert_eq!(steering.receive_prompt().await, "do not lose this");
}

#[test]
fn steering_requested_while_idle_becomes_a_queued_run() {
    let steering = TurnSteering::enabled();
    let receipt = steering
        .submit("start here".into(), TurnInputMode::Steer)
        .unwrap();
    assert_eq!(receipt.mode, TurnInputMode::Queue);
    assert_eq!(receipt.target_run, 1);
}
