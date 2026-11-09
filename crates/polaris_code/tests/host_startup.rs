//! Proof that another program can start `polaris` with its own plugins.
//!
//! Everything below is written the way a host crate has to write it: only
//! `polaris_code::` and framework paths, no copy of the binary's startup code,
//! and no `clap` dependency (the host builds its settings against the re-export).
//! A [`Host`] runs the real startup headless against a scripted model and a
//! temporary workspace, with one plugin that adds a code permission rule and one
//! that adds a step at each place in the coding loop, and the test checks:
//!
//! 1. with no host plugins the agent is the stock one: both edits run and no
//!    step runs;
//! 2. the rule's refusal reaches the model as the result of the refused call,
//!    the permitted edit still runs, and the file the rule protects is unchanged;
//! 3. the after-tools step ran once per tool round and its message reached the
//!    next request, and the before-finish step ran on each candidate answer and
//!    its hold made the model answer again;
//! 4. a host setting reads from its environment variable, and the command-line
//!    flag overrides it;
//! 5. a host closure that returns an error refuses the run with exit code `2`,
//!    and a command line that does not parse returns `2` instead of ending the
//!    process.
//!
//! The model is a scripted [`LlmProvider`] registered by a host plugin, and the
//! credential the stock startup checks for is a placeholder that is never sent
//! anywhere: nothing here reaches a network.
//!
//! This file holds exactly one test: it sets process-wide variables, and no
//! other test in the process may read the environment while it does. The
//! scenarios run in order inside it for the same reason.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt as _;
use polaris::models::llm::{
    ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider, LlmRequest,
    LlmResponse, LlmStream, Message, StopReason, StreamEvent, Usage,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::system::param::{Out, ResMut};
use polaris::system::plugin::{Contract, Plugin, PluginAccess, PluginId, Version, VersionReq};
use polaris::system::server::Server;
use polaris::system::system;
use polaris_code::agents::code_agent::steps::{CodeAgentSteps, FinishGate};
use polaris_code::agents::permission_rules::{
    PermissionRules, PermissionRulesPlugin, RuleDecision,
};
use polaris_code::agents::{CodeAgentPlugin, ContextManager};
use polaris_code::startup::{Host, clap};

/// The name the scripted provider registers under; the model id carries it.
const MODEL: &str = "scripted/test-model";

/// What the rule says when it refuses an edit.
const REFUSAL: &str = "host-proof: this file is read-only in this run";

/// The after-tools step's message to the model.
const TOOLS_NOTE: &str = "host-proof: a round of tool calls has finished";

/// The before-finish step's reason for not letting the first answer stand.
const HOLD: &str = "host-proof: check the notes file once more before finishing";

/// The answer the before-finish step holds, and the one it accepts.
const FIRST_ANSWER: &str = "first answer";
const FINAL_ANSWER: &str = "final answer";

/// How often each step ran, across the whole process.
static TOOL_ROUNDS: AtomicUsize = AtomicUsize::new(0);
static CANDIDATES: AtomicUsize = AtomicUsize::new(0);

#[system]
async fn note_the_tool_round(mut context: ResMut<ContextManager>) {
    TOOL_ROUNDS.fetch_add(1, Ordering::SeqCst);
    context.push(Message::user(TOOLS_NOTE));
}

#[system]
async fn check_the_candidate(answer: Out<LlmResponse>, mut gate: ResMut<FinishGate>) {
    CANDIDATES.fetch_add(1, Ordering::SeqCst);
    if answer.text() == FIRST_ANSWER {
        gate.hold(HOLD);
    }
}

/// Adds one code permission rule: the named file may not be edited.
struct Guard {
    protected: String,
}

impl Plugin for Guard {
    const ID: &'static str = "host_proof::guard";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<PermissionRulesPlugin>()]
    }

    fn access(&self) -> PluginAccess {
        PluginAccess::new()
            .extends::<PermissionRules>(VersionReq::caret(PermissionRules::CONTRACT_VERSION))
    }

    fn build(&self, server: &mut Server) {
        let protected = self.protected.clone();
        server
            .get_resource_mut::<PermissionRules>()
            .expect("PermissionRulesPlugin provides the rules")
            .add_code_rule("host-proof.protect", move |tool, args, _root| {
                let path = args.get("path")?.as_str()?;
                (tool == "edit_file" && path == protected)
                    .then(|| (RuleDecision::Deny, REFUSAL.to_string()))
            })
            .expect("the name is new and the server is still building");
    }
}

/// Adds one step after each round of tool calls and one before the agent
/// finishes.
struct Steps;

impl Plugin for Steps {
    const ID: &'static str = "host_proof::steps";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<CodeAgentPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        let mut steps = server
            .get_resource_mut::<CodeAgentSteps>()
            .expect("CodeAgentPlugin provides the step registry");
        steps
            .after_tools("host-proof.tools", 0, |graph| {
                graph.add_system(note_the_tool_round);
            })
            .expect("the key is new");
        steps
            .before_finish("host-proof.finish", 0, |graph| {
                graph.add_system(check_the_candidate);
            })
            .expect("the key is new");
    }
}

/// One scripted model reply.
enum Reply {
    /// Edit the first file, then the second, in one round of tool calls.
    Edits,
    /// Answer with this text.
    Text(&'static str),
}

/// The replies the model gives in order, and the requests it was sent.
#[derive(Default)]
struct Script {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<String>>,
}

struct Scripted(Arc<Script>);

impl LlmProvider for Scripted {
    fn name(&self) -> &'static str {
        "scripted"
    }

    async fn generate(&self, _: &str, _: LlmRequest) -> Result<LlmResponse, GenerationError> {
        panic!("the coding agent streams; recovery must not run");
    }

    async fn stream(&self, _: &str, request: LlmRequest) -> Result<LlmStream, GenerationError> {
        self.0
            .requests
            .lock()
            .unwrap()
            .push(format!("{:?}", request.messages));
        let reply = self
            .0
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("a scripted reply for every request");
        let mut events = Vec::new();
        let stop_reason = match reply {
            Reply::Text(text) => {
                events.extend([
                    StreamEvent::ContentBlockStart {
                        index: 0,
                        block: ContentBlockStartData::Text,
                    },
                    StreamEvent::ContentBlockDelta {
                        index: 0,
                        delta: ContentBlockDelta::Text(text.into()),
                    },
                    StreamEvent::ContentBlockStop { index: 0 },
                ]);
                StopReason::EndTurn
            }
            Reply::Edits => {
                for (index, (path, old, new)) in [
                    ("protected.txt", "beta", "gamma"),
                    ("notes.txt", "alpha", "omega"),
                ]
                .into_iter()
                .enumerate()
                {
                    let index = u32::try_from(index).unwrap();
                    events.extend([
                        StreamEvent::ContentBlockStart {
                            index,
                            block: ContentBlockStartData::ToolCall {
                                id: format!("edit-{index}"),
                                call_id: None,
                                name: "edit_file".into(),
                            },
                        },
                        StreamEvent::ContentBlockDelta {
                            index,
                            delta: ContentBlockDelta::ToolCall {
                                arguments: serde_json::json!({
                                    "path": path,
                                    "old_string": old,
                                    "new_string": new,
                                })
                                .to_string(),
                            },
                        },
                        StreamEvent::ContentBlockStop { index },
                    ]);
                }
                StopReason::ToolUse
            }
        };
        events.push(StreamEvent::MessageStop {
            stop_reason,
            usage: Usage {
                input_tokens: Some(100),
                output_tokens: Some(10),
                total_tokens: Some(110),
                ..Default::default()
            },
            id: None,
            model: None,
        });
        Ok(futures_util::stream::iter(events.into_iter().map(Ok)).boxed())
    }
}

/// Registers [`Scripted`] once the model registry exists, the seam the stock
/// providers use.
struct ScriptedModel(Arc<Script>);

impl Plugin for ScriptedModel {
    const ID: &'static str = "host_proof::scripted_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin builds first")
            .register_llm_provider(Scripted(self.0.clone()));
    }
}

/// The host a downstream crate would write: two settings of its own, and the
/// plugins they select, added on top of the stock startup.
fn host(script: &Arc<Script>) -> Host {
    let script = script.clone();
    Host::new()
        .arg(
            clap::Arg::new("protect")
                .long("protect")
                .env("HOST_PROOF_PROTECT")
                .value_name("PATH"),
        )
        .arg(
            clap::Arg::new("steps")
                .long("steps")
                .action(clap::ArgAction::SetTrue),
        )
        .plugins(move |launch, server| {
            assert!(launch.is_headless(), "every scenario here is unattended");
            assert!(launch.working_dir().is_absolute());
            server.add_plugins(ScriptedModel(script));
            if let Some(path) = launch.matches().get_one::<String>("protect") {
                server.add_plugins(Guard {
                    protected: path.clone(),
                });
            }
            if launch.matches().get_flag("steps") {
                server.add_plugins(Steps);
            }
            Ok(())
        })
}

/// A workspace with two files to edit.
fn workspace() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("protected.txt"), "beta\n").unwrap();
    std::fs::write(root.path().join("notes.txt"), "alpha\n").unwrap();
    root
}

fn script(replies: impl IntoIterator<Item = Reply>) -> Arc<Script> {
    Arc::new(Script {
        replies: Mutex::new(replies.into_iter().collect()),
        ..Script::default()
    })
}

/// The command line of one unattended run that may edit files.
fn command_line(root: &Path, extra: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = [
        "polaris",
        "--print",
        "Apply the edits.",
        "--allow-edit",
        "--model",
        MODEL,
        "--output-format",
        "json",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    args.push(root.display().to_string());
    args.extend(extra.iter().map(|arg| (*arg).to_string()));
    args
}

fn read(root: &Path, name: &str) -> String {
    std::fs::read_to_string(root.join(name)).unwrap()
}

/// Neither step runs, and both edits go through.
async fn without_host_plugins_the_agent_is_stock() {
    let root = workspace();
    let script = script([Reply::Edits, Reply::Text(FINAL_ANSWER)]);
    let rounds = TOOL_ROUNDS.load(Ordering::SeqCst);
    let candidates = CANDIDATES.load(Ordering::SeqCst);

    // Only the scripted model is added. `HOST_PROOF_PROTECT` is set in the
    // environment, but this host defines no such setting, so nothing reads it.
    let code = Host::new()
        .plugins({
            let script = script.clone();
            move |_, server| {
                server.add_plugins(ScriptedModel(script));
                Ok(())
            }
        })
        .run(command_line(root.path(), &[]))
        .await;

    assert_eq!(code, 0);
    assert_eq!(read(root.path(), "protected.txt"), "gamma\n");
    assert_eq!(read(root.path(), "notes.txt"), "omega\n");
    let requests = script.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "one tool round and then the answer");
    assert!(!requests[1].contains(TOOLS_NOTE));
    assert!(!requests[1].contains("Permission denied"));
    assert_eq!(TOOL_ROUNDS.load(Ordering::SeqCst), rounds);
    assert_eq!(CANDIDATES.load(Ordering::SeqCst), candidates);
}

/// A rule from the environment-supplied setting and both steps.
async fn the_rule_and_the_steps_take_part_in_the_run() {
    let root = workspace();
    let script = script([
        Reply::Edits,
        Reply::Text(FIRST_ANSWER),
        Reply::Text(FINAL_ANSWER),
    ]);
    let rounds = TOOL_ROUNDS.load(Ordering::SeqCst);
    let candidates = CANDIDATES.load(Ordering::SeqCst);

    // `HOST_PROOF_PROTECT=protected.txt` is in the environment.
    let code = host(&script)
        .run(command_line(root.path(), &["--steps"]))
        .await;

    assert_eq!(code, 0);
    // The rule refused the protected file and nothing else.
    assert_eq!(read(root.path(), "protected.txt"), "beta\n");
    assert_eq!(read(root.path(), "notes.txt"), "omega\n");
    let requests = script.requests.lock().unwrap();
    assert_eq!(requests.len(), 3, "the hold made the model answer again");
    // The refusal is what the model read as the result of the refused call.
    assert!(
        requests[1].contains("Call refused:") && requests[1].contains(REFUSAL),
        "the refusal did not reach the model: {}",
        requests[1]
    );
    assert!(requests[1].contains("Edited notes.txt"), "{}", requests[1]);
    // The after-tools step ran once, and its message is in the next request.
    assert_eq!(TOOL_ROUNDS.load(Ordering::SeqCst) - rounds, 1);
    assert!(requests[1].contains(TOOLS_NOTE));
    // The before-finish step saw both candidates; the hold reached the model
    // with the answer it held.
    assert_eq!(CANDIDATES.load(Ordering::SeqCst) - candidates, 2);
    assert!(requests[2].contains(FIRST_ANSWER) && requests[2].contains(HOLD));
    assert!(!requests[1].contains(HOLD), "nothing was held before then");
}

/// The command-line flag wins over the environment, and a run without steps
/// runs none.
async fn a_flag_overrides_the_environment() {
    let root = workspace();
    let script = script([Reply::Edits, Reply::Text(FINAL_ANSWER)]);
    let rounds = TOOL_ROUNDS.load(Ordering::SeqCst);

    let code = host(&script)
        .run(command_line(root.path(), &["--protect", "notes.txt"]))
        .await;

    assert_eq!(code, 0);
    assert_eq!(read(root.path(), "notes.txt"), "alpha\n", "the flag's file");
    assert_eq!(read(root.path(), "protected.txt"), "gamma\n");
    let requests = script.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains(REFUSAL));
    assert_eq!(TOOL_ROUNDS.load(Ordering::SeqCst), rounds, "no steps added");
}

/// Refusals return an exit code and leave the process running.
async fn refusals_and_usage_errors_return_exit_codes() {
    let root = workspace();
    let script = script([]);

    let checked = Arc::new(AtomicUsize::new(0));
    let seen = checked.clone();
    let code = Host::new()
        .configure(move |launch, options| {
            assert!(launch.is_headless());
            assert!(launch.working_dir().is_absolute());
            assert!(options.auto_route);
            assert!(options.model_request_timeout.is_none());
            options.auto_route = false;
            seen.fetch_add(1, Ordering::SeqCst);
            Err("early validation refused the host settings".into())
        })
        .plugins(|_, _| panic!("validation must precede plugin assembly"))
        .run(command_line(root.path(), &[]))
        .await;
    assert_eq!(code, 2);
    assert_eq!(checked.load(Ordering::SeqCst), 1);

    // A host timeout follows the same provider validation as the stock setting.
    let code = host(&script)
        .configure(|_, options| {
            options.model_request_timeout = Some("12".parse().unwrap());
            Ok(())
        })
        .run(command_line(root.path(), &["--provider", "anthropic"]))
        .await;
    assert_eq!(code, 2, "the host fallback must reach provider validation");
    assert!(script.requests.lock().unwrap().is_empty());

    // A closure that returns an error refuses the run.
    let code = Host::new()
        .plugins(|_, _| Err("the host's own configuration is wrong".into()))
        .run(command_line(root.path(), &[]))
        .await;
    assert_eq!(code, 2, "an unattended run's configuration error");
    assert_eq!(read(root.path(), "notes.txt"), "alpha\n");
    assert!(script.requests.lock().unwrap().is_empty());

    // A flag nobody defined is a usage error, returned rather than exited.
    let code = host(&script)
        .run(command_line(root.path(), &["--no-such-flag"]))
        .await;
    assert_eq!(code, 2);

    // A host setting is validated by the same parser as the stock flags.
    let code = host(&script)
        .run(command_line(root.path(), &["--steps=maybe"]))
        .await;
    assert_eq!(code, 2);

    // Help and version succeed, and the host's setting is listed.
    assert_eq!(host(&script).run(["polaris", "--help"]).await, 0);
    assert_eq!(host(&script).run(["polaris", "--version"]).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_starts_polaris_with_its_own_settings_and_plugins() {
    let config = tempfile::tempdir().unwrap();
    // SAFETY: this is the only test in the process, and it sets the variables
    // before anything else runs.
    unsafe {
        // Keep the user's own configuration and credentials out of the run.
        std::env::set_var("POLARIS_CONFIG_DIR", config.path());
        for name in [
            "POLARIS_CODE_PROVIDER",
            "POLARIS_CODE_REGION",
            "POLARIS_CODE_MAX_COST_USD",
            "POLARIS_CODE_MAX_TOKENS",
            "POLARIS_CODE_PERMISSION_MODE",
            "POLARIS_CODE_PERMISSION_CEILING",
            "POLARIS_CODE_COMPOSITION_POLICY",
            "POLARIS_CODE_GATE_PLANNER",
            "POLARIS_CODE_DOMAIN",
            "POLARIS_CODE_ARM",
            "POLARIS_CODE_TOPOLOGY_PIN",
        ] {
            std::env::remove_var(name);
        }
        // The stock startup checks that the default provider has a key. This is
        // a placeholder: the scripted model answers, so nothing sends it.
        std::env::set_var("ANTHROPIC_API_KEY", "placeholder-never-sent");
        std::env::set_var("POLARIS_CODE_MODELS", MODEL);
        std::env::set_var("POLARIS_CODE_MODEL", MODEL);
        std::env::set_var("HOST_PROOF_PROTECT", "protected.txt");
    }

    without_host_plugins_the_agent_is_stock().await;
    the_rule_and_the_steps_take_part_in_the_run().await;
    a_flag_overrides_the_environment().await;
    refusals_and_usage_errors_return_exit_codes().await;
}
