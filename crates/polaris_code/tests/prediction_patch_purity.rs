//! A prediction patch contains the agent's change to the subject repository
//! and nothing else.
//!
//! The SWE-bench adapter builds one patch per episode by staging the whole
//! working tree and diffing it against the instance's pinned base commit
//! (`_collect_patch` in `evals/packs/swebench_verified/environment.py`). Anything the
//! agent writes inside the working directory is therefore a candidate for the
//! prediction: before this proof existed, every patch carried
//! `.polaris/sessions/<id>.json` — the whole conversation, the problem
//! statement included — and `.polaris/rewind-v1.json`.
//!
//! This file boots the server the way `main.rs`'s `--print` branch does, with
//! a scripted model in place of `AnthropicPlugin` so no key and no network are
//! involved, runs one turn whose single tool call is a real `edit_file`, saves
//! the session as `run_headless` does, and then collects the patch with the
//! adapter's own two commands. The assertions are that the patch names exactly
//! the edited file, and that the session and the journal are still on disk —
//! invisible to git is the fix, not "no longer written".

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::StreamExt as _;
use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, StopReason, StreamEvent, Usage,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{IOContent, IOMessage, IOProvider, PersistenceAPI, PersistencePlugin};
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::shell::{ShellConfig, ShellPlugin};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code::agents::{
    AgentConfig, CodeAgent, CodeAgentPlugin, PermissionMode, PolicySnapshot, UnattendedGrants,
};
use polaris_code::config::ModelCatalog;
use polaris_code::exec::{ExecIOPlugin, ExecIOProvider};
use polaris_code::run::{self, TurnReason};
use polaris_code::state;
use polaris_code::terminal::{
    AttachmentStore, BackgroundTasks, EditJournal, SessionSettings, TurnSteering,
};
use polaris_code::tools::{CodeGraphPlugin, CodeToolsConfig, CodeToolsPlugin};
use polaris_code::usage::TokenMeter;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use serde_json::json;

/// The model id the scripted provider answers for.
const MODEL: &str = "patch-purity-scripted/model";

/// The one file the subject repository ships at its base commit.
const SUBJECT_FILE: &str = "solver.py";

/// Its content at the base commit.
const BEFORE: &str = "def solve():\n    return 1\n";

/// What the agent's one `edit_file` call replaces.
const OLD_STRING: &str = "    return 1";

/// And what it replaces it with.
const NEW_STRING: &str = "    return 42";

// ─────────────────────────────────────────────────────────────────────────────
// The scripted model: one `edit_file` call, then an answer
// ─────────────────────────────────────────────────────────────────────────────

struct ScriptedProvider {
    calls: AtomicUsize,
}

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "patch-purity-scripted"
    }

    async fn generate(&self, _: &str, _: LlmRequest) -> Result<LlmResponse, GenerationError> {
        // Only `recover` generates. A turn that lands here has taken the error
        // path, which the test's `TurnReason` assertion catches.
        Ok(LlmResponse {
            content: vec![AssistantBlock::text("recovered")],
            usage: Usage::default(),
            stop_reason: StopReason::EndTurn,
            id: None,
            model: None,
        })
    }

    async fn stream(&self, _: &str, _: LlmRequest) -> Result<LlmStream, GenerationError> {
        let events = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let args = json!({
                "path": SUBJECT_FILE,
                "old_string": OLD_STRING,
                "new_string": NEW_STRING,
                "replace_all": false,
            });
            vec![
                Ok(StreamEvent::ContentBlockStart {
                    index: 0,
                    block: ContentBlockStartData::ToolCall {
                        id: "call-edit".into(),
                        call_id: None,
                        name: "edit_file".into(),
                    },
                }),
                Ok(StreamEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::ToolCall {
                        arguments: args.to_string(),
                    },
                }),
                Ok(StreamEvent::ContentBlockStop { index: 0 }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::ToolUse,
                    usage: Usage::default(),
                    id: None,
                    model: None,
                }),
            ]
        } else {
            vec![
                Ok(StreamEvent::ContentBlockStart {
                    index: 0,
                    block: ContentBlockStartData::Text,
                }),
                Ok(StreamEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::Text("edited solver.py".into()),
                }),
                Ok(StreamEvent::ContentBlockStop { index: 0 }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                    id: None,
                    model: None,
                }),
            ]
        };
        Ok(futures_util::stream::iter(events).boxed())
    }
}

struct ScriptedModelPlugin;

impl Plugin for ScriptedModelPlugin {
    const ID: &'static str = "test::patch_purity_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin builds first")
            .register_llm_provider(ScriptedProvider {
                calls: AtomicUsize::new(0),
            });
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// git, driven the way the adapter drives it
// ─────────────────────────────────────────────────────────────────────────────

/// Run one git command in `dir` and return its stdout, failing loudly.
///
/// The machine's own git configuration is switched off and identity is passed
/// per invocation, so a developer's global `core.excludesFile` can neither
/// hide a leak nor be blamed for one, and nothing outside `dir` is written.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args([
            "-c",
            "user.email=proof@example.invalid",
            "-c",
            "user.name=proof",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("this proof needs `git` on PATH: {err}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("git prints UTF-8 here")
}

/// Create the subject repository: one committed file, nothing else, exactly
/// the clean pinned tree an episode starts from. Returns the base commit.
fn subject_repository(dir: &Path) -> String {
    std::fs::write(dir.join(SUBJECT_FILE), BEFORE).unwrap();
    git(dir, &["init", "-q"]);
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "base"]);
    assert_eq!(
        git(dir, &["status", "--porcelain"]),
        "",
        "an episode refuses to start unless the tree is clean"
    );
    git(dir, &["rev-parse", "HEAD"]).trim().to_string()
}

/// `git add -A` then `git diff --cached <base>`, which is `_collect_patch` in
/// `evals/packs/swebench_verified/environment.py` with the container runtime and the
/// `--workdir` prefix removed.
fn collect_patch(dir: &Path, base: &str) -> (Vec<String>, String) {
    git(dir, &["add", "-A"]);
    let names = git(dir, &["diff", "--cached", "--name-only", base])
        .lines()
        .map(str::to_string)
        .collect();
    (names, git(dir, &["diff", "--cached", base]))
}

// ─────────────────────────────────────────────────────────────────────────────
// The host, as `main.rs` assembles it for `--print`
// ─────────────────────────────────────────────────────────────────────────────

/// Run one non-interactive turn over `working_dir` and save the session, the
/// way `run_headless` does.
async fn run_one_turn(working_dir: &Path) -> TurnReason {
    // `main.rs` opens the state directory, then the journal, then points the
    // session store at `<state>/sessions`.
    let state_dir = state::state_dir(working_dir).unwrap();
    let journal = EditJournal::open(working_dir).unwrap();

    let (provider, input, mut output) = ExecIOProvider::new(64);
    let provider = Arc::new(provider);
    tokio::spawn(async move { while output.recv().await.is_some() {} });

    let mut server = Server::new();
    server.insert_global(TokenMeter::new());
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(ScriptedModelPlugin)
        .add_plugins(ToolsPlugin)
        .add_plugins(CodeToolsPlugin::new(
            CodeToolsConfig::new(working_dir).with_edit_journal(journal),
        ))
        .add_plugins(OxigraphPlugin::in_memory().with_tools(true))
        .add_plugins(CodeGraphPlugin::new(working_dir))
        // One deviation from `main.rs`, and it makes the proof stricter: the
        // shell's overflow cache defaults to `~/.polaris`, outside any
        // workspace. Pointing it inside the workspace here both keeps the test
        // out of the developer's home directory and shows that a cache that
        // *is* workspace-local still never reaches a patch.
        .add_plugins(ShellPlugin::new(
            ShellConfig::new()
                .with_working_dir(working_dir)
                .with_allowed_dirs(vec![working_dir.to_path_buf()])
                .with_cache_dir(state_dir.join("shell-cache")),
        ))
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
            state_dir.join("sessions"),
        ))))
        .add_plugins(CodeAgentPlugin::new())
        .add_plugins(ExecIOPlugin::new(provider.clone()));
    server.finish().await.unwrap();

    let persistence = server.api::<PersistenceAPI>().unwrap();
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.set_serializers(persistence.serializers());
    sessions.register_agent(CodeAgent).unwrap();

    // The unattended posture `--print --allow-edit` installs: `edit_file` runs
    // without a confirm round trip, everything else fails closed.
    let mode = PermissionMode::Unattended(UnattendedGrants {
        edit_file: true,
        run_command_patterns: Vec::new(),
    });
    let mut policy = PolicySnapshot::defaults();
    policy.permission = mode.permission_policy();

    let session = SessionId::new();
    let config = AgentConfig::new(MODEL, working_dir.to_path_buf());
    let settings = SessionSettings::new(MODEL, mode, &ModelCatalog::pinned(MODEL));
    let steering = TurnSteering::default();
    let attachments = AttachmentStore::new();
    let (tasks, _backgrounded) = BackgroundTasks::new(Duration::from_secs(120));
    sessions
        .create_session_with_executor(
            server.create_context(),
            &session,
            &AgentTypeId::from_name(CodeAgent::NAME),
            polaris::graph::GraphExecutor::new().with_default_max_iterations(25),
            move |ctx| {
                ctx.insert(config.clone());
                ctx.insert(settings.clone());
                ctx.insert(steering.clone());
                ctx.insert(attachments.clone());
                ctx.insert(tasks.clone());
            },
        )
        .unwrap();

    // `CodeAgent::setup` seeds its own defaults while creating the session, so
    // the run's policy is applied afterwards and wholesale — exactly the order
    // `main.rs` uses, and for the reason recorded there.
    sessions
        .with_context(&session, move |ctx| {
            ctx.insert(policy.clone());
        })
        .await
        .unwrap();

    input
        .send(IOMessage::from_external(
            "cli",
            IOContent::Text("make solve() return 42".to_string()),
        ))
        .await
        .unwrap();
    drop(input);

    let outcome = run::drive_turn(sessions, &session).await;
    provider.close().await;
    sessions.save_session(&session).await.unwrap();
    outcome.reason
}

/// Where this crate sits relative to the pack that collects a prediction.
fn environment_py() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../evals/packs/swebench_verified/environment.py")
}

// ─────────────────────────────────────────────────────────────────────────────
// The proofs
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_collected_prediction_patch_carries_only_the_subject_repository_change() {
    let root = tempfile::tempdir().unwrap();
    let working_dir = root.path().canonicalize().unwrap();
    let base = subject_repository(&working_dir);

    let reason = run_one_turn(&working_dir).await;
    assert_eq!(
        reason,
        TurnReason::Completed,
        "the scripted turn runs the real `edit_file` and ends"
    );
    assert_eq!(
        std::fs::read_to_string(working_dir.join(SUBJECT_FILE)).unwrap(),
        BEFORE.replace(OLD_STRING, NEW_STRING),
        "the agent's edit landed"
    );

    // The agent did write its state — this is the part that must stay true.
    // A benchmark run is debugged from these files.
    let sessions_dir = working_dir.join(".polaris/sessions");
    let saved: Vec<_> = std::fs::read_dir(&sessions_dir)
        .expect("the session store directory exists")
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(saved.len(), 1, "one saved session: {saved:?}");
    let transcript = std::fs::read_to_string(&saved[0]).unwrap();
    assert!(
        transcript.contains("make solve() return 42"),
        "the saved session is the real transcript, still readable"
    );
    assert!(
        working_dir.join(".polaris/rewind-v1.json").is_file(),
        "the rewind journal is still written"
    );

    // And none of it is visible to git. Read the working-tree status before
    // staging, so both views of the same claim are available below.
    let status = git(&working_dir, &["status", "--porcelain"]);

    let (names, patch) = collect_patch(&working_dir, &base);
    assert_eq!(
        names,
        vec![SUBJECT_FILE.to_string()],
        "the prediction patch names exactly the edited file"
    );
    assert!(
        !patch.contains(".polaris"),
        "no agent state reached the patch:\n{patch}"
    );
    assert!(
        !patch.contains("make solve() return 42"),
        "the problem statement never reaches a prediction:\n{patch}"
    );
    assert!(
        patch.contains(NEW_STRING),
        "the agent's change is present:\n{patch}"
    );
    assert_eq!(
        status,
        format!(" M {SUBJECT_FILE}\n"),
        "and the run left the working tree with one dirty path"
    );
}

#[test]
fn the_adapter_still_collects_a_patch_the_way_this_proof_does() {
    // A drift guard, not a duplicate: if `_collect_patch` ever stops staging
    // the whole tree, or diffs something other than the base commit, the two
    // commands this file mirrors are no longer the ones a prediction is built
    // from and the proof above stops meaning anything.
    let source = std::fs::read_to_string(environment_py())
        .expect("the dataset pack lives next to this crate in the repository");
    for expected in [
        r#""git", "add", "-A""#,
        r#""git", "diff", "--cached", base_commit"#,
    ] {
        assert!(
            source.contains(expected),
            "environment.py no longer collects a patch with `{expected}`"
        );
    }
}
