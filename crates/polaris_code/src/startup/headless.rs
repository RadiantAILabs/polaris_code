//! The non-interactive front end: one turn, then the result on stdout.

use std::io::Write;
use std::sync::Arc;

use polaris::plugins::{IOContent, IOMessage, IOProvider};
use polaris::sessions::{SessionId, SessionsAPI};
use tokio::sync::mpsc;

use crate::exec::{ExecIOProvider, OutputFormat, RunReport, stream_json_line, usage_message};
use crate::run;

/// Drive the one non-interactive turn: enqueue the prompt if there is one,
/// run it, drain and emit the result per `format`, save the session, and
/// return the process exit code.
///
/// `prompt` is `None` for a `--topology-task` run, which has nothing to say to
/// the model: the work is already in the queue, so the selection point at the
/// top of the turn answers it and returns before `process_turn_with` is
/// reached.
///
/// This is the front end, not the emitter: every byte it writes is rendered by
/// [`RunReport`] / [`stream_json_line`], which are library-side and tested.
/// Stdout carries exactly the result (`text`/`json`) or exactly the message
/// stream (`stream-json`, printed by the concurrent drain below); the identity
/// triple and every diagnostic go to stderr.
///
/// Both machine-readable formats carry the run's token and cost accounting:
/// `json` as the `usage` member of its object, `stream-json` as the final
/// message on the stream (`io_type = "usage"`). `text` reserves stdout for the
/// assistant's answer, so a consumer that needs accounting uses `json`.
///
/// A run given `--max-cost-usd` / `--max-tokens` that reached its limit
/// returns [`CEILING_EXIT_CODE`](run::CEILING_EXIT_CODE) and states what it
/// spent against what it was allowed on stderr; `json` also carries the same
/// facts as its `ceiling` member. The session is saved first, exactly as for
/// any other ending.
pub(super) async fn run_headless(
    sessions: &SessionsAPI,
    session_id: &SessionId,
    prompt: Option<String>,
    exec_io: (
        Arc<ExecIOProvider>,
        mpsc::Sender<IOMessage>,
        mpsc::Receiver<IOMessage>,
    ),
    format: OutputFormat,
    max_output_tokens: u32,
) -> i32 {
    let (provider, input_tx, mut output_rx) = exec_io;
    let status = run::StatusEmitter::new(provider.clone());

    // Enqueue before the turn starts: `receive_user_input` swallows a closed
    // channel rather than erroring, so a send racing the turn could hang.
    if let Some(prompt) = prompt {
        let _ = input_tx
            .send(IOMessage::from_external("cli", IOContent::Text(prompt)))
            .await;
    }
    drop(input_tx);

    // Drain concurrently so `stream-json` prints as messages arrive rather
    // than after the turn completes. `text`/`json` need nothing from this
    // stream but must still drain it so the bounded output channel never
    // backpressures the turn.
    let drain = tokio::spawn(async move {
        while let Some(message) = output_rx.recv().await {
            if format.streams_messages() {
                match stream_json_line(&message) {
                    Ok(line) => println!("{line}"),
                    Err(err) => eprintln!("polaris: failed to serialize a message: {err}"),
                }
            }
        }
    });

    let turn = run::drive_turn_with_status(sessions, session_id, Some(&status));
    tokio::pin!(turn);
    let mut interrupted = false;
    let outcome = loop {
        tokio::select! {
            outcome = &mut turn => break outcome,
            signal = run::stop_signal() => {
                if interrupted {
                    // Deliberate hard escape: unlike the cooperative first
                    // signal, a second one does not wait or save.
                    std::process::exit(signal.exit_code());
                }
                // A supervisor's SIGTERM is an orderly stop request, so it
                // takes the same cooperative route as SIGINT: cancel the
                // turn, then fall through to the save and the report below.
                interrupted = run::request_interrupt(sessions, session_id);
            }
        }
    };

    // `stream-json` closes with the run's own accounting, so a streaming
    // consumer reads exactly the versioned object `json` embeds instead of
    // summing the live `io_type=tokens` display deltas. Sent through the
    // provider rather than printed here, so the drain renders it with the same
    // line renderer as every other message and it lands last, in stream order.
    if format.streams_messages()
        && let Some(usage) = outcome.usage.as_ref()
    {
        let _ = provider.send(usage_message(usage)).await;
    }

    // Let the drain task see end-of-stream, then wait for every stream-json
    // line to be flushed before printing the result / returning.
    provider.close().await;
    let _ = drain.await;

    if let Err(message) = run::persist_turn(sessions, session_id).await {
        eprintln!("polaris: {message}");
    }

    let report = RunReport::new(session_id, &outcome, max_output_tokens);
    // Identity on stderr for every format, so `text` and `stream-json` stdout
    // stay exactly the result / the message stream; `json` embeds the same
    // three identifiers in its object, since its stdout *is* the result.
    eprintln!("{}", report.identity_line());
    if let Some(line) = report.model_line() {
        eprintln!("{line}");
    }
    if let Some(line) = report.error_line() {
        eprintln!("{line}");
    }
    // What was spent against what was allowed, for every format: `text` and
    // `stream-json` have no structured place to carry it, and a run that
    // stopped early must say why even to a caller reading only stderr.
    if let Some(line) = report.ceiling_line() {
        eprintln!("{line}");
    }
    if let Some(payload) = report.stdout_payload(format) {
        println!("{payload}");
    }
    let _ = std::io::stdout().flush();

    report.exit_code()
}
