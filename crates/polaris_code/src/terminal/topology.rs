//! Handing the harness one piece of coding work, and telling the person who
//! submitted it what happened to it.
//!
//! `/topology` lets a person put work into the [`CodingTaskQueue`]; the
//! selection point in [`crate::run::drive_turn_with_status`] reads it on the
//! next turn. The sealed catalog, coding pack, evidence capability, and queue
//! are installed by `main.rs`.
//!
//! This module is that affordance, composed from three things that already
//! existed rather than a new layer over them:
//!
//! | Primitive | Used for |
//! |---|---|
//! | [`CodingTaskQueue`] | the resource with a public `accept`; this module is one more caller of it |
//! | [`CommandRegistry`] | additive `&self` registration, so `/topology` is a local action like `/rewind` |
//! | the selection point | untouched: accepted work is read by the *next* turn, where selection already runs |
//!
//! There is no topology manager, no task router, and no second entry point
//! into the run surface. [`TopologyWork::accept`] is the only place that writes
//! to the queue, and the turn that consumes it is the one
//! `drive_turn_with_status` was already going to run.
//!
//! # What a user-supplied coding task is
//!
//! A [`CodingTask`] is a sandbox root, one or more exact find/replace edits (or
//! a statement in place of them), a declared scope, and a success check — the
//! stated edits landing, a stub marker's absence, an external command exiting
//! zero, or an external verifier. A user has a sentence. This module
//! deliberately does **not** bridge
//! that gap by inference, and the reason is a contract rather than a
//! preference: a topology run is a pristine context that does not call the
//! model (`docs/topology-selection.md`), so there is nowhere inside the
//! topology runtime for "turn this sentence into an edit" to happen. Deriving
//! the fields *before* acceptance would mean a model call on the conversational
//! path whose only purpose is to feed the non-conversational one, and it would
//! make the accepted task a guess that the evidence episode then records as
//! fact.
//!
//! `/topology` accepts only work a user can state exactly:
//!
//! ```text
//! /topology file=src/lib.rs find="a + 0" replace="a" intent=careful
//! ```
//!
//! Every field is the user's own; nothing is inferred from prose and nothing is
//! defaulted that would change what is written to disk. A second edit, a
//! third, and so on take numbered fields (`file2=`/`find2=`/`replace2=`,
//! onward); `scope=<path>[,<path>...]` declares which files the work
//! concerns; and `command=<program> [command-args=<arg>[,<arg>...]]` states an
//! external command as the success check instead of the stated edits
//! themselves. [`TopologyWork::prepare`]'s own `FIELDS`/`SYNTAX` constants and
//! `docs/topology-selection.md` are the authoritative grammar; this doc
//! comment only motivates the shape. Two consequences worth knowing before
//! building on this shape:
//!
//! - **The queue carries the accepted task.** Exact fields produce an
//!   [`AcceptedCodingTask`], and selection consumes that task from the same
//!   queue.
//! - **The spec is the unit of authorization.** Because the user wrote the
//!   exact bytes, the permission gate below can bind to them (see
//!   [`TopologyWork::prepare`]). The accepted spec records what the human
//!   agreed to.
//!
//! # Permission posture
//!
//! A topology performs real sandboxed edits through
//! [`crate::cosmos::edit`], inside a context that has no `UserIO` and therefore
//! cannot ask anything. The posture is bound at acceptance instead, and to the
//! same resolution `edit_file` gets — [`PermissionPolicy::resolve`] over the
//! registry's `Confirm` default:
//!
//! | Resolved `edit_file` | What `/topology` does |
//! |---|---|
//! | [`ToolPermission::Allow`] (auto-edit, or `--allow-edit`) | accepts immediately |
//! | [`ToolPermission::Confirm`] (the interactive default) | shows the exact edit and accepts only after `/topology confirm` |
//! | [`ToolPermission::Deny`] (plan mode, or unattended without `--allow-edit`) | refuses, and says how to change it |
//!
//! The confirm posture is the interesting one: a per-edit prompt is impossible
//! inside a topology run, so the confirmation moves to the only place it can
//! be, which is before the work is accepted at all. Nothing is skipped — the
//! human sees the file, the search text and the replacement, and a second
//! explicit command is required before anything is queued.
//!
//! [`PermissionPolicy::resolve`]: crate::agents::PermissionPolicy::resolve

use std::fmt;
use std::iter::Peekable;
use std::path::PathBuf;
use std::str::Chars;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use polaris::plugins::{IO_TYPE, IOContent, IOMessage};
use polaris::tools::ToolPermission;
use polaris_cosmos::contracts::{CandidateConsideration, DurationMs, SelectionRationale, Status};
use polaris_cosmos::topology::TopologyCatalogSnapshot;
use polaris_tui::commands::CommandSpec;

use crate::agents::PermissionMode;
use crate::cosmos::compare::{ArmOutcome, ComparisonReport, compare_intents};
use crate::cosmos::host::{AcceptedCodingTask, CodingTaskQueue, TopologyDirective, TopologyTurn};
use crate::cosmos::task::{CodingIntent, CodingObjective, CodingTask, ExactEdit, SuccessCheck};
use crate::run::TurnOutcome;

use super::{CommandContext, CommandError, CommandFuture, CommandOutcome, CommandRegistry};

/// The tool whose posture a topology run is bound to.
const EDIT_TOOL: &str = "edit_file";

/// `edit_file`'s registered permission, which the policy resolves over.
const EDIT_REGISTRY_DEFAULT: ToolPermission = ToolPermission::Confirm;

/// Stub marker assumed when the spec does not state one.
const DEFAULT_STUB_MARKER: &str = "todo!()";

/// Longest slug a derived task reference carries, so a long path cannot push
/// the reference past the 128-byte Cosmos stable-name limit.
const REFERENCE_SLUG_MAX: usize = 64;

/// Highest edit index `fileN=`/`findN=`/`replaceN=` accepts, alongside the
/// unindexed `file`/`find`/`replace` for the first edit. Keep in sync with
/// the indexed-field hint in [`TopologyWork::prepare`]'s unknown-field error.
const MAX_EDITS: usize = 8;

/// The spec fields `/topology` accepts, in the order it documents them.
/// `file`/`find`/`replace` also accept the numbered forms `file2`/`find2`/
/// `replace2` and onward, up to [`MAX_EDITS`], for a task with more than one
/// exact edit; those numbered forms are not listed here individually.
const FIELDS: &[&str] = &[
    "statement",
    "file",
    "find",
    "replace",
    "stub",
    "scope",
    "command",
    "command-args",
    "intent",
    "verifier",
    "ref",
    "budget-ms",
    "topology",
];

/// One line of `key=value` spec syntax, for every message that has to teach it.
const SYNTAX: &str = "statement=<problem> [intent=fast|careful|thorough|answer] \
     [scope=<path>[,<path>...]] [stub=<marker>] \
     [command=<program> [command-args=<arg>[,<arg>...]]] \
     [ref=<name>] [budget-ms=<n>] [topology=auto|none|<registration-id>]\n     \
     OR file=<path> find=<text> replace=<text> \
     [file2=<path> find2=<text> replace2=<text> ...] \
     [stub=<marker>] [intent=fast|careful|thorough] [scope=<path>[,<path>...]] \
     [command=<program> [command-args=<arg>[,<arg>...]]] \
     [verifier=deterministic|comparative] [ref=<name>] [budget-ms=<n>] \
     [topology=auto|none|<registration-id>]";

/// The spec fields `/topology compare` accepts. `intents` replaces `intent`:
/// a comparison states two, not one.
const COMPARE_FIELDS: &[&str] = &[
    "file",
    "find",
    "replace",
    "stub",
    "intents",
    "verifier",
    "ref",
    "budget-ms",
];

/// `/topology compare` spec syntax. `answer` is reachable here even though the
/// single-edit spec above refuses it: comparing "edit it" against "just
/// decide whether it needs an edit" is exactly the kind of pair this
/// affordance exists to show. It does **not** reliably produce one refused
/// arm: `code.tree-question` needs no source-mutation capability, so it stays
/// eligible under `CodingObjective::new(Answer)` and actually runs, reaching
/// its own real (if unresolved) conclusion rather than a refusal — pairing
/// `answer` with an edit intent on the same task instead shows two genuinely
/// different real outcomes for one edit, which is the literal feature this
/// affordance exists to show. The only combination this policy reliably
/// refuses is `verifier=comparative`, which excludes every registration
/// uniformly and so refuses both arms together, not divergently.
const COMPARE_SYNTAX: &str = "intents=<a>,<b> file=<path> find=<text> replace=<text> \
     [stub=<marker>] [verifier=deterministic|comparative] [ref=<name>] [budget-ms=<n>] \
     (each of a, b is one of fast, careful, thorough, answer)";

/// Why the harness would not accept a submitted piece of coding work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkRefused {
    /// The spec could not be read as `key=value` fields.
    Malformed(String),
    /// The spec was read but the coding task itself was rejected.
    Rejected(String),
    /// The session's permission posture does not permit a topology run.
    Posture(String),
}

impl WorkRefused {
    /// The refusal text, without its kind.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::Malformed(detail) | Self::Rejected(detail) | Self::Posture(detail) => detail,
        }
    }
}

impl fmt::Display for WorkRefused {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(detail) => write!(formatter, "malformed coding work: {detail}"),
            Self::Rejected(detail) => write!(formatter, "the coding task was rejected: {detail}"),
            Self::Posture(detail) => write!(formatter, "{detail}"),
        }
    }
}

impl std::error::Error for WorkRefused {}

/// A parsed, sandbox-checked, posture-checked piece of coding work that has
/// not been queued yet.
///
/// Holding this is *not* holding a place in the queue: only
/// [`TopologyWork::accept`] writes to it, and only one call per prepared value,
/// because `accept` takes it by value.
#[derive(Debug, Clone)]
pub struct PreparedWork {
    task: CodingTask,
    objective: CodingObjective,
    reference: String,
    confirm_first: bool,
    directive: TopologyDirective,
}

impl PreparedWork {
    /// Whether the posture owes an explicit confirmation before this is
    /// accepted.
    #[must_use]
    pub const fn needs_confirmation(&self) -> bool {
        self.confirm_first
    }

    /// The task reference the evidence episode will be keyed by.
    #[must_use]
    pub fn reference(&self) -> &str {
        &self.reference
    }

    /// The exact work, rendered for a human to agree to.
    #[must_use]
    pub fn preview(&self) -> String {
        let scope_line = if self.task.scope().is_empty() {
            String::new()
        } else {
            format!("\n- scope: {}", self.task.scope().join(", "))
        };
        let edits = self.task.edits();
        match edits {
            [] => format!(
                "Coding work `{}` is ready to run:\n\n\
                 - statement: `{}`\n\
                 - intent: `{}`\n\
                 - success: {}{scope_line}\n\
                 - topology: {}",
                self.reference,
                self.task.statement(),
                intent_label(self.task.intent()),
                success_check_label(self.task.success()),
                directive_label(&self.directive),
            ),
            [edit] => format!(
                "Coding work `{}` is ready to run:\n\n\
                 - file: `{}`\n\
                 - intent: `{}`\n\
                 - success: {}{scope_line}\n\
                 - topology: {}\n\n\
                 Replace:\n```\n{}\n```\nWith:\n```\n{}\n```",
                self.reference,
                edit.file(),
                intent_label(self.task.intent()),
                success_check_label(self.task.success()),
                directive_label(&self.directive),
                edit.find(),
                edit.replace(),
            ),
            edits => {
                let mut rendered = format!(
                    "Coding work `{}` is ready to run, across {} edits:\n\n\
                     - intent: `{}`\n\
                     - success: {}{scope_line}\n\
                     - topology: {}\n",
                    self.reference,
                    edits.len(),
                    intent_label(self.task.intent()),
                    success_check_label(self.task.success()),
                    directive_label(&self.directive),
                );
                for (index, edit) in edits.iter().enumerate() {
                    rendered.push_str(&format!(
                        "\n{}. `{}` — replace:\n```\n{}\n```\nwith:\n```\n{}\n```\n",
                        index + 1,
                        edit.file(),
                        edit.find(),
                        edit.replace(),
                    ));
                }
                rendered
            }
        }
    }
}

/// The `preview`/receipt rendering of one topology directive.
fn directive_label(directive: &TopologyDirective) -> String {
    match directive {
        TopologyDirective::Automatic => "left to the selector".to_owned(),
        TopologyDirective::Only(name) => format!("`{name}` (asked for by name)"),
        TopologyDirective::None => "none (you asked for no topology to run)".to_owned(),
    }
}

/// The `preview`/receipt rendering of one success check.
fn success_check_label(check: &SuccessCheck) -> String {
    match check {
        SuccessCheck::StatedEdits { marker: None } => "the stated edit(s) landing".to_owned(),
        SuccessCheck::StatedEdits {
            marker: Some(marker),
        } => format!("the stated edit(s) landing, plus a clean scan for `{marker}`"),
        SuccessCheck::Command(command) => {
            let mut line = command.program().to_owned();
            for arg in command.args() {
                line.push(' ');
                line.push_str(arg);
            }
            format!("command `{line}` exiting zero")
        }
        SuccessCheck::Delegated { verifier } => format!("judged externally by `{verifier}`"),
    }
}

/// What accepting one piece of coding work put into the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedWork {
    /// Task reference the decision and its evidence are keyed by.
    pub reference: String,
    /// Target file, relative to the sandbox root.
    pub file: String,
    /// Requested intent's label.
    pub intent: &'static str,
    /// How many exact edits the accepted task carries. `0` for a statement
    /// task with none pre-computed.
    pub edit_count: usize,
    /// How many accepted tasks are now waiting for a selection point.
    pub queued: usize,
    /// What the submission said about which topology should run it.
    pub directive: TopologyDirective,
}

impl AcceptedWork {
    /// The receipt shown to whoever submitted the work.
    #[must_use]
    pub fn summary(&self) -> String {
        let position = if self.queued > 1 {
            format!(" It is number {} in the queue.", self.queued)
        } else {
            String::new()
        };
        let next_turn = match &self.directive {
            TopologyDirective::Automatic => "The next turn selects a topology for it.".to_owned(),
            TopologyDirective::Only(name) => {
                format!("The next turn runs `{name}`, or refuses and says why.")
            }
            TopologyDirective::None => {
                "The next turn runs no topology for it, as requested.".to_owned()
            }
        };
        let on = if self.edit_count > 1 {
            format!(
                "across {} edits starting with `{}`",
                self.edit_count, self.file
            )
        } else {
            format!("on `{}`", self.file)
        };
        format!(
            "Accepted coding work `{}` {on} ({} intent). {next_turn}{position}",
            self.reference, self.intent
        )
    }
}

/// The host handle `/topology` and `--topology-task` submit work through.
///
/// Cloning shares one handle. It owns no session state and never borrows the
/// session context, which is what lets a command handler run while a turn is
/// parked inside `receive`: the queue is its own lock, and the posture is read
/// from the host's settings rather than from the session's resource table.
#[derive(Clone)]
pub struct TopologyWork {
    queue: CodingTaskQueue,
    working_dir: PathBuf,
    posture: Arc<dyn Fn() -> PermissionMode + Send + Sync>,
    pending: Arc<Mutex<Option<PreparedWork>>>,
    submissions: Arc<AtomicU64>,
    /// The sealed catalog snapshot, captured once. Sealing is final, so one
    /// read cannot go stale — the same reasoning `TopologyListing` already
    /// relies on. Naming a topology is validated against this, at acceptance,
    /// rather than left to fail a turn later.
    catalog: TopologyCatalogSnapshot,
    comparisons: Arc<AtomicU64>,
}

impl fmt::Debug for TopologyWork {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TopologyWork")
            .field("working_dir", &self.working_dir)
            .field("queued", &self.queue.len())
            .finish_non_exhaustive()
    }
}

impl TopologyWork {
    /// Builds the handle over the host's own queue.
    ///
    /// `posture` is read on every submission rather than captured once, so a
    /// `/permission` change between submissions is respected. `catalog` is the
    /// same sealed snapshot the composition root reads back after
    /// `Server::finish` for `/topologies` — naming a topology here is
    /// validated against it before anything is queued.
    #[must_use]
    pub fn new(
        queue: CodingTaskQueue,
        working_dir: impl Into<PathBuf>,
        catalog: TopologyCatalogSnapshot,
        posture: impl Fn() -> PermissionMode + Send + Sync + 'static,
    ) -> Self {
        Self {
            queue,
            working_dir: working_dir.into(),
            posture: Arc::new(posture),
            pending: Arc::new(Mutex::new(None)),
            submissions: Arc::new(AtomicU64::new(0)),
            catalog,
            comparisons: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Registers `/topology` on the terminal's command registry.
    ///
    /// # Errors
    ///
    /// Refuses a registry that already has a `/topology`.
    pub fn register(&self, commands: &CommandRegistry) -> Result<(), CommandError> {
        let work = self.clone();
        commands.register(
            CommandSpec::new(
                "topology",
                "Submit one exact coding edit to the topology runtime, or compare two intents",
            )
            .with_usage("[confirm|cancel|compare <spec>|<spec>]"),
            Arc::new(move |ctx: CommandContext<'_>| {
                let work = work.clone();
                let args = ctx.args.to_string();
                Box::pin(async move { work.command(&args).await }) as CommandFuture<'_>
            }),
        )
    }

    /// Reads one spec into work this session is permitted to run.
    ///
    /// Nothing is queued: the returned value has to be handed to
    /// [`accept`](Self::accept).
    ///
    /// # Errors
    ///
    /// Returns [`WorkRefused::Malformed`] for a spec that is not `key=value`
    /// fields or is missing a required one, [`WorkRefused::Rejected`] when the
    /// coding task or its target file is refused, and
    /// [`WorkRefused::Posture`] when the session's permission mode would not
    /// let `edit_file` run at all.
    pub fn prepare(&self, spec: &str) -> Result<PreparedWork, WorkRefused> {
        let fields = split_fields(spec).map_err(WorkRefused::Malformed)?;
        let mut statement = None;
        let mut file = None;
        let mut find = None;
        let mut replace = None;
        let mut stub = None;
        let mut scope = None;
        let mut command = None;
        let mut command_args = None;
        let mut intent = CodingIntent::Careful;
        let mut comparative = false;
        let mut reference = None;
        let mut budget = None;
        let mut topology = None;
        let mut extra_edits: Vec<(Option<String>, Option<String>, Option<String>)> =
            vec![(None, None, None); MAX_EDITS - 1];
        for (key, value) in fields {
            let slot: &mut Option<String> = match key.as_str() {
                "statement" => &mut statement,
                "file" => &mut file,
                "find" => &mut find,
                "replace" => &mut replace,
                "stub" => &mut stub,
                "scope" => &mut scope,
                "command" => &mut command,
                "command-args" => &mut command_args,
                "ref" => &mut reference,
                "topology" => &mut topology,
                "intent" => {
                    intent = match parse_intent(&value) {
                        Some(parsed) => parsed,
                        None => {
                            return Err(WorkRefused::Malformed(format!(
                                "intent `{value}` is not one of fast, careful, thorough or answer"
                            )));
                        }
                    };
                    continue;
                }
                "verifier" => {
                    comparative = match value.as_str() {
                        "deterministic" => false,
                        "comparative" => true,
                        other => {
                            return Err(WorkRefused::Malformed(format!(
                                "verifier `{other}` is not one of deterministic or comparative"
                            )));
                        }
                    };
                    continue;
                }
                "budget-ms" => {
                    budget = Some(value.parse::<u64>().map_err(|_| {
                        WorkRefused::Malformed(format!("budget-ms `{value}` is not a whole number"))
                    })?);
                    continue;
                }
                other => match indexed_edit_field(other) {
                    Some((index, IndexedEditSlot::File)) => &mut extra_edits[index].0,
                    Some((index, IndexedEditSlot::Find)) => &mut extra_edits[index].1,
                    Some((index, IndexedEditSlot::Replace)) => &mut extra_edits[index].2,
                    None => {
                        return Err(WorkRefused::Malformed(format!(
                            "`{other}` is not a field of a coding task; known fields are {}, \
                             or file2/find2/replace2 through file{MAX_EDITS}/find{MAX_EDITS}/\
                             replace{MAX_EDITS} for additional edits",
                            FIELDS.join(", ")
                        )));
                    }
                },
            };
            if slot.replace(value).is_some() {
                return Err(WorkRefused::Malformed(format!(
                    "field `{key}` was given twice"
                )));
            }
        }

        let extra_edits_present = extra_edits
            .iter()
            .any(|(file, find, replace)| file.is_some() || find.is_some() || replace.is_some());
        let exact_fields_present =
            file.is_some() || find.is_some() || replace.is_some() || extra_edits_present;
        if statement.is_some() && exact_fields_present {
            return Err(WorkRefused::Malformed(format!(
                "`statement` is an alternative to file/find/replace; usage: /topology {SYNTAX}"
            )));
        }
        if statement.is_none() && intent == CodingIntent::Answer {
            return Err(WorkRefused::Malformed(
                "intent `answer` requires a statement task; exact edits must use fast, careful or thorough".into(),
            ));
        }
        let missing: Vec<&str> = ["file", "find", "replace"]
            .into_iter()
            .zip([file.is_none(), find.is_none(), replace.is_none()])
            .filter_map(|(name, absent)| absent.then_some(name))
            .collect();
        if statement.is_none() && !missing.is_empty() {
            return Err(WorkRefused::Malformed(format!(
                "missing required field(s) {}; usage: /topology {SYNTAX}",
                missing.join(", ")
            )));
        }
        let file = file.unwrap_or_default();
        let find = find.unwrap_or_default();
        let replace = replace.unwrap_or_default();
        if statement.is_none() && find.is_empty() {
            return Err(WorkRefused::Malformed(
                "find must not be empty; a topology replaces an exact string".into(),
            ));
        }

        // Every numbered edit past the first is either fully stated or absent
        // entirely: a partial triple names an edit the caller can't have
        // meant to submit.
        let mut extra_edit_values: Vec<(String, String, String)> = Vec::new();
        for (offset, (file, find, replace)) in extra_edits.into_iter().enumerate() {
            let edit_number = offset + 2;
            if file.is_none() && find.is_none() && replace.is_none() {
                continue;
            }
            let missing: Vec<&str> = [("file", &file), ("find", &find), ("replace", &replace)]
                .into_iter()
                .filter_map(|(name, value)| value.is_none().then_some(name))
                .collect();
            if !missing.is_empty() {
                return Err(WorkRefused::Malformed(format!(
                    "edit {edit_number} is missing field(s) {}; file{edit_number}/find{edit_number}/\
                     replace{edit_number} must be given together",
                    missing.join(", ")
                )));
            }
            let find = find.unwrap_or_default();
            if find.is_empty() {
                return Err(WorkRefused::Malformed(format!(
                    "edit {edit_number}: find must not be empty; a topology replaces an exact string"
                )));
            }
            extra_edit_values.push((file.unwrap_or_default(), find, replace.unwrap_or_default()));
        }

        if command_args.is_some() && command.is_none() {
            return Err(WorkRefused::Malformed(
                "`command-args` requires `command`".into(),
            ));
        }
        if command.is_some() && stub.is_some() {
            return Err(WorkRefused::Malformed(
                "`stub` and `command` are alternative success checks; only one may be given".into(),
            ));
        }
        let command_success = match command {
            Some(program) => {
                let args = command_args
                    .as_deref()
                    .map(split_comma_list)
                    .unwrap_or_default();
                Some(
                    SuccessCheck::command(program, args)
                        .map_err(|error| WorkRefused::Rejected(error.to_string()))?,
                )
            }
            None => None,
        };
        let scope_paths = scope.as_deref().map(split_comma_list).unwrap_or_default();

        let ordinal = self.submissions.fetch_add(1, Ordering::Relaxed) + 1;
        let reference = reference.unwrap_or_else(|| {
            let subject = statement.as_deref().unwrap_or(&file);
            derive_reference("user", subject, ordinal)
        });
        let task = if let Some(statement) = statement {
            let question = statement.clone();
            let success = match command_success.clone() {
                Some(success) => success,
                None => match stub.clone().filter(|marker| !marker.is_empty()) {
                    Some(marker) => SuccessCheck::stated_edits_with_marker(marker)
                        .map_err(|error| WorkRefused::Rejected(error.to_string()))?,
                    None => SuccessCheck::delegated("external-verifier")
                        .expect("a fixed verifier name is valid"),
                },
            };
            CodingTask::from_statement(
                reference.clone(),
                self.working_dir.clone(),
                statement,
                intent,
                success,
            )
            .map(|task| {
                let task = task.with_scope(scope_paths.clone());
                if intent == CodingIntent::Answer {
                    task.with_question(question)
                } else {
                    task
                }
            })
        } else {
            (|| {
                let mut edits = vec![ExactEdit::new(file, find, replace)?];
                for (file, find, replace) in extra_edit_values {
                    edits.push(ExactEdit::new(file, find, replace)?);
                }
                let success = match command_success.clone() {
                    Some(success) => success,
                    None => {
                        let marker = stub
                            .clone()
                            .unwrap_or_else(|| DEFAULT_STUB_MARKER.to_string());
                        if marker.is_empty() {
                            SuccessCheck::stated_edits()
                        } else {
                            SuccessCheck::stated_edits_with_marker(marker)?
                        }
                    }
                };
                let statement = if edits.len() == 1 {
                    format!("apply the stated edit to {}", edits[0].file())
                } else {
                    format!("apply {} stated edits", edits.len())
                };
                CodingTask::from_statement(
                    reference.clone(),
                    self.working_dir.clone(),
                    statement,
                    intent,
                    success,
                )
                .map(|task| task.with_edits(edits).with_scope(scope_paths.clone()))
            })()
        }
        .map_err(|error| WorkRefused::Rejected(error.to_string()))?;
        let task = match budget {
            Some(budget) => task.with_latency_budget(DurationMs::new(budget)),
            None => task,
        };

        // Resolve every target through the *same* sandbox check the edit
        // action will use, so work aimed outside the workspace is refused
        // here rather than by a run that had to start to find out. Only
        // reachability is checked: whether the search text is present,
        // unique, or already replaced is the topology's own business, and
        // `AlreadySatisfied` handles the tree that already meets the request
        // better than a parser could.
        for edit in task.edits() {
            let target = edit.file();
            match task.tools_config().resolve_path(target) {
                None => {
                    return Err(WorkRefused::Rejected(format!(
                        "`{target}` escapes the workspace `{}`",
                        self.working_dir.display()
                    )));
                }
                Some(resolved) if !resolved.is_file() => {
                    return Err(WorkRefused::Rejected(format!(
                        "`{target}` is not a file in the workspace `{}`",
                        self.working_dir.display()
                    )));
                }
                Some(_) => {}
            }
        }
        // A declared scope is a belief about the work, not a target that must
        // already exist, so only escaping the sandbox is refused here.
        for scoped in task.scope() {
            if task.tools_config().resolve_path(scoped).is_none() {
                return Err(WorkRefused::Rejected(format!(
                    "`{scoped}` escapes the workspace `{}`",
                    self.working_dir.display()
                )));
            }
        }

        let objective = if comparative {
            CodingObjective::new(intent).requiring_comparative_verifier()
        } else {
            CodingObjective::new(intent)
        };
        let directive = self.resolve_directive(topology.as_deref())?;
        let confirm_first = match self.edit_posture() {
            ToolPermission::Allow => false,
            ToolPermission::Confirm => true,
            ToolPermission::Deny => {
                return Err(WorkRefused::Posture(format!(
                    "this session may not run `{EDIT_TOOL}`, and a topology edits files \
                     without being able to ask; nothing was accepted. \
                     Leave plan mode with `/permission auto-edit`, or grant `--allow-edit` \
                     to an unattended run."
                )));
            }
        };
        Ok(PreparedWork {
            task,
            objective,
            reference,
            confirm_first,
            directive,
        })
    }

    /// Reads the `topology=` field into a directive, validated against the
    /// sealed catalog this handle was built with.
    ///
    /// `none` and `auto` are the two reserved values; anything else must name
    /// a registration the catalog actually holds — checked here, at
    /// acceptance, so a typo is refused immediately rather than by a turn that
    /// had to run to find out. This is "unknown", the one refusal ground the
    /// selection point cannot produce on its own: it only ever sees a
    /// directive that already named something real.
    ///
    /// # Errors
    ///
    /// Returns [`WorkRefused::Malformed`] when `requested` names neither a
    /// reserved value nor a registered topology.
    fn resolve_directive(&self, requested: Option<&str>) -> Result<TopologyDirective, WorkRefused> {
        match requested {
            None | Some("auto") => Ok(TopologyDirective::Automatic),
            Some("none") => Ok(TopologyDirective::None),
            Some(name) => {
                let known: Vec<&str> = self
                    .catalog
                    .candidates()
                    .iter()
                    .map(|candidate| candidate.registration_id().as_str())
                    .collect();
                if known.contains(&name) {
                    Ok(TopologyDirective::Only(name.to_owned()))
                } else {
                    Err(WorkRefused::Malformed(format!(
                        "`{name}` is not a registered topology; known topologies are {} \
                         (see /topologies), or use `none`/`auto`",
                        known.join(", ")
                    )))
                }
            }
        }
    }

    /// Puts prepared work into the queue for the next turn's selection point.
    ///
    /// Taking `prepared` by value is the whole safety property: one prepared
    /// submission cannot be queued twice, so a confirmation cannot be replayed
    /// into two accepted tasks.
    pub fn accept(&self, prepared: PreparedWork) -> AcceptedWork {
        let intent = intent_label(prepared.task.intent());
        let edit_count = prepared.task.edits().len();
        let file = prepared
            .task
            .edits()
            .first()
            .map_or_else(|| "statement".to_owned(), |edit| edit.file().to_owned());
        let reference = prepared.reference;
        let directive = prepared.directive;
        self.queue.accept(
            AcceptedCodingTask::new(prepared.task)
                .with_objective(prepared.objective)
                .with_topology_directive(directive.clone()),
        );
        AcceptedWork {
            reference,
            file,
            intent,
            edit_count,
            queued: self.queue.len(),
            directive,
        }
    }

    /// Reads one spec and queues it in a single step, for a launch flag that
    /// has no second command to confirm with.
    ///
    /// # Errors
    ///
    /// As [`prepare`](Self::prepare), plus [`WorkRefused::Posture`] when the
    /// posture owes a confirmation this path cannot ask for.
    pub fn accept_spec(&self, spec: &str) -> Result<AcceptedWork, WorkRefused> {
        let prepared = self.prepare(spec)?;
        if prepared.needs_confirmation() {
            return Err(WorkRefused::Posture(format!(
                "this posture confirms every `{EDIT_TOOL}` before it runs, and a launch flag \
                 has nobody to ask; nothing was accepted. Submit it with `/topology` in an \
                 interactive session, or grant `--allow-edit` to an unattended run."
            )));
        }
        Ok(self.accept(prepared))
    }

    /// How many accepted tasks are waiting for a selection point.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// The permission `edit_file` resolves to under the live posture.
    fn edit_posture(&self) -> ToolPermission {
        (self.posture)()
            .permission_policy()
            .resolve(EDIT_TOOL, EDIT_REGISTRY_DEFAULT)
    }

    /// `/topology [confirm|cancel|compare <spec>|<spec>]`.
    async fn command(&self, args: &str) -> CommandOutcome {
        let args = args.trim();
        match args {
            "" => return CommandOutcome::Reply(self.status()),
            "confirm" => return self.confirm(),
            "cancel" => {
                return match self.take_pending() {
                    Some(prepared) => CommandOutcome::Reply(format!(
                        "Discarded the pending coding work `{}`. Nothing was queued.",
                        prepared.reference()
                    )),
                    None => CommandOutcome::Error(
                        "there is no pending coding work to cancel".to_string(),
                    ),
                };
            }
            _ => {}
        }
        if let Some(rest) = args.strip_prefix("compare")
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            return self.compare(rest.trim()).await;
        }
        match self.prepare(args) {
            Err(refused) => CommandOutcome::Error(refused.to_string()),
            Ok(prepared) if prepared.needs_confirmation() => {
                let preview = prepared.preview();
                let replaced = self
                    .pending
                    .lock()
                    .map(|mut slot| slot.replace(prepared).is_some())
                    .unwrap_or(false);
                let note = if replaced {
                    "\n\nThis replaced an earlier pending submission."
                } else {
                    ""
                };
                CommandOutcome::Reply(format!(
                    "{preview}\n\nThis session confirms every `{EDIT_TOOL}` before it runs, and a \
                     topology cannot ask mid-run, so nothing is queued yet. Run \
                     `/topology confirm` to accept exactly this edit, or `/topology cancel` to \
                     drop it.{note}"
                ))
            }
            // The posture already allows `edit_file` without asking, so the
            // submission is the authorization. `Control` ends this idle turn so
            // the driver's next turn reaches the selection point.
            Ok(prepared) => CommandOutcome::Control(self.accept(prepared).summary()),
        }
    }

    /// `/topology confirm`.
    fn confirm(&self) -> CommandOutcome {
        let Some(prepared) = self.take_pending() else {
            return CommandOutcome::Error(format!(
                "there is no pending coding work to confirm; submit it first with \
                 `/topology {SYNTAX}`"
            ));
        };
        // Re-read the posture: `/permission plan` between the proposal and the
        // confirmation must win over the earlier check.
        if self.edit_posture() == ToolPermission::Deny {
            return CommandOutcome::Error(format!(
                "this session may no longer run `{EDIT_TOOL}`, so the pending coding work \
                 `{}` was dropped rather than queued.",
                prepared.reference()
            ));
        }
        CommandOutcome::Control(self.accept(prepared).summary())
    }

    /// `/topology` with no arguments.
    fn status(&self) -> String {
        let queued = self.queue.len();
        let posture = match self.edit_posture() {
            ToolPermission::Allow => "accepts submitted work immediately".to_string(),
            ToolPermission::Confirm => {
                format!(
                    "shows the edit and waits for `/topology confirm` (`{EDIT_TOOL}` is confirmed in this session)"
                )
            }
            ToolPermission::Deny => {
                format!("refuses submitted work (`{EDIT_TOOL}` is denied in this session)")
            }
        };
        let pending = match self.pending.lock() {
            Ok(slot) => slot
                .as_ref()
                .map(|prepared| format!("\n\nPending confirmation:\n\n{}", prepared.preview()))
                .unwrap_or_default(),
            Err(_) => String::new(),
        };
        format!(
            "Topology work queued for the next turn: {queued}. This session {posture}.\n\n\
             Submit one exact edit with:\n\n```\n/topology {SYNTAX}\n```\n\n\
             `find` and `replace` are exact text — quote them (`\"…\"`) to include spaces, and \
             use `\\n` inside the quotes for a line break. Nothing is inferred from prose: a \
             topology runs without the model, so it can only be handed work that is already \
             stated exactly.{pending}"
        )
    }

    /// `/topology compare <spec>`, using [`COMPARE_SYNTAX`].
    ///
    /// Runs immediately and asks nothing: neither arm ever touches this
    /// session's real workspace — each runs against its own disposable copy
    /// (see [`crate::cosmos::compare`]) — so the `edit_file` posture that
    /// gates a real submission does not apply here. Nothing is queued, no
    /// turn ends, and no comparison evidence reaches this session's own
    /// evidence store; this is a self-contained report, not a step in the
    /// accepted-work queue.
    async fn compare(&self, spec: &str) -> CommandOutcome {
        let parsed = match self.parse_compare_spec(spec) {
            Ok(parsed) => parsed,
            Err(refused) => return CommandOutcome::Error(refused.to_string()),
        };
        let report = compare_intents(
            &parsed.reference,
            &self.working_dir,
            &parsed.file,
            &parsed.find,
            &parsed.replace,
            &parsed.stub,
            parsed.budget.map(DurationMs::new),
            parsed.comparative,
            parsed.intents,
        )
        .await;
        CommandOutcome::Reply(render_comparison(&report))
    }

    /// Reads one `/topology compare` spec. Never touches the workspace: file
    /// reachability and edit satisfiability are each arm's own business,
    /// checked independently against its own isolated copy
    /// (`ArmOutcome::SetupFailed` reports it if refused).
    ///
    /// # Errors
    ///
    /// [`WorkRefused::Malformed`] for anything [`split_fields`] rejects, an
    /// unknown or duplicate field, a non-numeric `budget-ms`, an empty
    /// `find`, or an `intents` value that is not exactly two comma-separated
    /// labels from [`parse_intent`]'s vocabulary.
    fn parse_compare_spec(&self, spec: &str) -> Result<CompareSpec, WorkRefused> {
        let fields = split_fields(spec).map_err(WorkRefused::Malformed)?;
        let mut file = None;
        let mut find = None;
        let mut replace = None;
        let mut stub = None;
        let mut intents = None;
        let mut comparative = false;
        let mut reference = None;
        let mut budget = None;
        for (key, value) in fields {
            let slot = match key.as_str() {
                "file" => &mut file,
                "find" => &mut find,
                "replace" => &mut replace,
                "stub" => &mut stub,
                "ref" => &mut reference,
                "intents" => &mut intents,
                "verifier" => {
                    comparative = match value.as_str() {
                        "deterministic" => false,
                        "comparative" => true,
                        other => {
                            return Err(WorkRefused::Malformed(format!(
                                "verifier `{other}` is not one of deterministic or comparative"
                            )));
                        }
                    };
                    continue;
                }
                "budget-ms" => {
                    budget = Some(value.parse::<u64>().map_err(|_| {
                        WorkRefused::Malformed(format!("budget-ms `{value}` is not a whole number"))
                    })?);
                    continue;
                }
                other => {
                    return Err(WorkRefused::Malformed(format!(
                        "`{other}` is not a field of a comparison; known fields are {}",
                        COMPARE_FIELDS.join(", ")
                    )));
                }
            };
            if slot.replace(value).is_some() {
                return Err(WorkRefused::Malformed(format!(
                    "field `{key}` was given twice"
                )));
            }
        }

        let missing: Vec<&str> = ["file", "find", "replace", "intents"]
            .into_iter()
            .zip([
                file.is_none(),
                find.is_none(),
                replace.is_none(),
                intents.is_none(),
            ])
            .filter_map(|(name, absent)| absent.then_some(name))
            .collect();
        if !missing.is_empty() {
            return Err(WorkRefused::Malformed(format!(
                "missing required field(s) {}; usage: /topology compare {COMPARE_SYNTAX}",
                missing.join(", ")
            )));
        }
        let file = file.unwrap_or_default();
        let find = find.unwrap_or_default();
        let replace = replace.unwrap_or_default();
        if find.is_empty() {
            return Err(WorkRefused::Malformed(
                "find must not be empty; a topology replaces an exact string".into(),
            ));
        }

        let intents = intents.unwrap_or_default();
        let labels: Vec<&str> = intents.split(',').map(str::trim).collect();
        if labels.len() != 2 {
            return Err(WorkRefused::Malformed(format!(
                "intents `{intents}` must name exactly two, comma-separated \
                 (fast, careful, thorough or answer)"
            )));
        }
        let parse_one = |label: &str| {
            parse_intent(label).ok_or_else(|| {
                WorkRefused::Malformed(format!(
                    "intent `{label}` is not one of fast, careful, thorough or answer"
                ))
            })
        };
        let intents = [parse_one(labels[0])?, parse_one(labels[1])?];

        let ordinal = self.comparisons.fetch_add(1, Ordering::Relaxed) + 1;
        let reference = reference.unwrap_or_else(|| derive_reference("compare", &file, ordinal));

        Ok(CompareSpec {
            intents,
            file,
            find,
            replace,
            stub: stub.unwrap_or_else(|| DEFAULT_STUB_MARKER.to_string()),
            reference,
            comparative,
            budget,
        })
    }

    fn take_pending(&self) -> Option<PreparedWork> {
        self.pending.lock().ok()?.take()
    }
}

/// Which of a numbered edit's three fields a spec key names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexedEditSlot {
    File,
    Find,
    Replace,
}

/// Reads a numbered edit field (`file2`, `find3`, `replace8`, ...) into its
/// zero-based index among the edits after the first, and which of the three
/// slots it names.
///
/// `None` for the unindexed `file`/`find`/`replace` (edit 1, handled by their
/// own match arms), for a suffix that is not a whole number, and for an index
/// outside `2..=MAX_EDITS`.
fn indexed_edit_field(key: &str) -> Option<(usize, IndexedEditSlot)> {
    let (slot, suffix) = if let Some(suffix) = key.strip_prefix("file") {
        (IndexedEditSlot::File, suffix)
    } else if let Some(suffix) = key.strip_prefix("find") {
        (IndexedEditSlot::Find, suffix)
    } else {
        let suffix = key.strip_prefix("replace")?;
        (IndexedEditSlot::Replace, suffix)
    };
    let index: usize = suffix.parse().ok()?;
    (2..=MAX_EDITS)
        .contains(&index)
        .then_some((index - 2, slot))
}

/// Splits a `scope=`/`command-args=` value on commas, trimming and dropping
/// empty entries so a trailing comma or repeated separators are harmless.
fn split_comma_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A parsed `/topology compare` spec, not yet run.
#[derive(Debug)]
struct CompareSpec {
    intents: [CodingIntent; 2],
    file: String,
    find: String,
    replace: String,
    stub: String,
    reference: String,
    comparative: bool,
    budget: Option<u64>,
}

/// The panel message a finished turn owes whoever submitted coding work.
///
/// `None` for an ordinary turn: a conversational turn reports nothing here, so
/// "no topology was eligible" can never be confused with "no topology work was
/// submitted" — the first is a message, the second is silence.
#[must_use]
pub fn turn_report(outcome: &TurnOutcome) -> Option<IOMessage> {
    let topology = outcome.topology.as_ref()?;
    let text = render_topology_turn(topology);
    Some(
        IOMessage::from_external("/topology", IOContent::Text(text))
            .with_metadata(IO_TYPE, "message"),
    )
}

/// Renders one [`TopologyTurn`] the way a live session's panel message does.
///
/// Shared by [`turn_report`] (one live selection) and [`render_arm_outcome`]
/// (one comparison arm that reached a real answer) so the two surfaces never
/// describe the same outcome shape in two different sentences.
fn render_topology_turn(topology: &TopologyTurn) -> String {
    match topology {
        TopologyTurn::CostCapped(run) => format!(
            "**Cost cap reached** for coding work `{}`. The current workspace edits are kept.\n\n- topology: `{}`\n- termination: `cost-cap`",
            run.task_id.id(),
            run.registration_id
        ),
        TopologyTurn::Executed(run) => {
            let mut passed = 0;
            let mut issues = 0;
            let mut other = 0;
            for observation in &run.observations {
                match observation.status() {
                    Status::Pass => passed += 1,
                    Status::Issue => issues += 1,
                    _ => other += 1,
                }
            }
            let others: Vec<&CandidateConsideration> = run
                .candidates
                .iter()
                .filter(|candidate| {
                    candidate.candidate().registration_id().as_str() != run.registration_id
                })
                .collect();
            let alternatives = if others.is_empty() {
                "none — it was the only registered candidate".to_owned()
            } else {
                others
                    .iter()
                    .map(|candidate| candidate_summary(candidate))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let composition = run.composition.as_ref().map_or_else(
                || "not recorded (legacy topology result)".to_owned(),
                |evidence| {
                    let plan = evidence.decision().choice().plan();
                    format!(
                        "`{}` via `{}` [{}]",
                        plan.composition_digest(),
                        plan.recipe().recipe(),
                        plan.blocks()
                            .iter()
                            .map(|block| block.block().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                },
            );
            let gate_plan = run
                .gate_plan_trail
                .as_ref()
                .map_or_else(String::new, |trail| {
                    format!("\n - gate plan: {}", gate_plan_summary(trail))
                });
            let exhausted_gate_plan =
                run.exhausted_gate_plan_trail
                    .as_ref()
                    .map_or_else(String::new, |trail| {
                        format!(
                            "\n - gate planner: exhausted after {} refused attempt(s), fell back \
                             to the finite recipe grammar",
                            trail.revisions().len()
                        )
                    });
            format!(
                "**A topology ran** for coding work `{}`.\n\n\
                 - topology: `{}`{}{}\n\
                 - composition: {composition}{gate_plan}{exhausted_gate_plan}\n\
                 - graph: {} node(s) in {} ms\n\
                 - conclusion: {}\n\
                 - why this one: {}\n\
                 - alternatives considered: {alternatives}\n\
                 - verifier observations: {passed} pass, {issues} issue, {other} other\n\
                 - decision: `{}`\n\n\
                 The workspace was edited by the topology, not by the conversational agent, \
                 and this turn did not advance the conversation.",
                run.task_id.id(),
                run.registration_id,
                if run.fallback_position == 0 {
                    String::new()
                } else {
                    format!(" (fallback position {})", run.fallback_position)
                },
                if run.requested.is_some() {
                    " — you asked for this topology"
                } else {
                    ""
                },
                run.nodes_executed,
                run.duration.as_millis(),
                topology.answer(),
                rationale_summary(&run.rationale),
                run.decision_id,
            )
        }
        TopologyTurn::NoEligibleCandidate(unstarted) => match &unstarted.requested {
            Some(requested) => format!(
                "**`{requested}` could not be honoured** for coding work `{}`.\n\n\
                 You asked for it by name; it was refused on its own ground, before anything \
                 else was considered — nothing ran in its place.\n\n\
                 - exclusion codes: {}\n\
                 - decision: `{}`\n\n\
                 Your workspace is unchanged. The decision is recorded as evidence; ask for a \
                 different topology, or leave it to the selector, with `/topology`.",
                unstarted.task_id.id(),
                reason_list(&unstarted.reasons),
                unstarted.decision_id,
            ),
            None => format!(
                "**No topology was eligible** for coding work `{}`.\n\n\
                 It was considered and refused: every registered topology was ranked against the \
                 objective and none passed every hard gate, so nothing was started and no default \
                 graph ran in its place.\n\n\
                 - exclusion codes: {}\n\
                 - decision: `{}`\n\n\
                 Your workspace is unchanged. The decision is recorded as evidence; submit \
                 different work with `/topology` when you are ready.",
                unstarted.task_id.id(),
                reason_list(&unstarted.reasons),
                unstarted.decision_id,
            ),
        },
        TopologyTurn::NotStarted(unstarted) => match &unstarted.requested {
            Some(requested) => format!(
                "**`{requested}` did not start** for coding work `{}`.\n\n\
                 You asked for it by name; it passed selection but was refused at live \
                 readmission, so nothing ran.\n\n\
                 - exclusion codes: {}\n\
                 - decision: `{}`\n\n\
                 Your workspace is unchanged. The refused attempt is recorded as evidence.",
                unstarted.task_id.id(),
                reason_list(&unstarted.reasons),
                unstarted.decision_id,
            ),
            None => format!(
                "**No topology started** for coding work `{}`.\n\n\
                 A topology was selected, but neither it nor any captured fallback survived live \
                 readmission, so nothing ran.\n\n\
                 - attempted: {}\n\
                 - exclusion codes: {}\n\
                 - decision: `{}`\n\n\
                 Your workspace is unchanged. Every refused attempt is recorded as evidence.",
                unstarted.task_id.id(),
                reason_list(&unstarted.attempted),
                reason_list(&unstarted.reasons),
                unstarted.decision_id,
            ),
        },
        TopologyTurn::Declined(declined) => format!(
            "**No topology ran** for coding work `{}`, because you asked for none.\n\n\
             The task was accepted and is recorded as declined; no selection was made, nothing \
             was appended to the topology evidence store, and your workspace is unchanged.",
            declined.task_id.id(),
        ),
    }
}

/// Renders one finished [`ComparisonReport`] for `/topology compare`.
///
/// Both arms are named regardless of how either one ended — a refused or
/// failed arm is reported the same as an executed one, never dropped or
/// treated as an error that swallows the other arm's result — and no verdict
/// is computed: the reader is shown what happened to each intent and left to
/// judge which they prefer.
fn render_comparison(report: &ComparisonReport) -> String {
    let mut text = format!(
        "**Topology comparison** `{}` — two isolated runs, one per intent.\n\n\
         Each arm ran against its own disposable copy of `{}`; neither touched your \
         workspace, and neither run's evidence was written to this session's own \
         evidence store.\n",
        report.reference,
        report.source_tree.display(),
    );
    for (index, arm) in report.arms.iter().enumerate() {
        text.push_str(&format!(
            "\n---\n\n### Arm {} — intent `{}`\n\n{}\n",
            index + 1,
            intent_label(arm.intent),
            render_arm_outcome(&arm.outcome)
        ));
    }
    text
}

/// Renders one comparison arm's outcome.
///
/// A real answer is rendered exactly as a live turn's would be
/// ([`render_topology_turn`]); a refusal before or during setup is rendered
/// as a plain, non-alarming statement of what happened, since a refused arm
/// is a result this feature exists to show, not an error.
fn render_arm_outcome(outcome: &ArmOutcome) -> String {
    match outcome {
        ArmOutcome::Selected(topology) => render_topology_turn(topology),
        ArmOutcome::SelectionFailed(error) => {
            format!("**This arm did not reach a topology.** Selection reported: {error}")
        }
        ArmOutcome::SetupFailed(detail) => {
            format!("**This arm did not reach a topology.** {detail}")
        }
    }
}

/// Renders the gate-plan revision trail an executed gate-plan composition
/// carries: the admitted plan's own shape and content digests, its
/// one-based revision-loop position, and how many earlier attempts this
/// turn's planner submitted and had refused before it. Nothing here is
/// recomputed — every value is read straight off the trail's own final,
/// admitted revision.
fn gate_plan_summary(trail: &polaris_cosmos::contracts::CompositionRevisionTrail) -> String {
    let Some(admitted) = trail.revisions().last() else {
        return "no revision recorded".to_owned();
    };
    let Some(record) = admitted.decision().gate_plan() else {
        return "no revision recorded".to_owned();
    };
    let prior = admitted.revision_attempt().get().saturating_sub(1);
    let revised = if prior == 0 {
        String::new()
    } else {
        format!(", after {prior} refused attempt(s)")
    };
    format!(
        "plan `{}` (shape `{}`), attempt {}{revised}",
        record.plan_digest(),
        record.shape().digest(),
        admitted.revision_attempt().get(),
    )
}

/// Renders one non-winning candidate: its rank if it was eligible, or the
/// exclusion codes that set it aside otherwise. Straight from the decision's
/// own [`CandidateConsideration`] — nothing here is re-scored or re-ranked.
fn candidate_summary(candidate: &CandidateConsideration) -> String {
    let id = candidate.candidate().registration_id().as_str();
    if let Some(rank) = candidate.rank() {
        return format!("`{id}` (rank {rank})");
    }
    let codes: Vec<String> = candidate
        .mechanical_admission()
        .reasons()
        .iter()
        .chain(candidate.policy_eligibility().reasons())
        .map(|reason| format!("`{}`", reason.code().as_str()))
        .collect();
    if codes.is_empty() {
        format!("`{id}` (not selected)")
    } else {
        format!("`{id}` (excluded: {})", codes.join(", "))
    }
}

/// Renders why the winner beat the runner-up: the decision's own precomposed
/// display summary when it recorded one, else its decisive score components
/// and any tie-break or exploration flags. Never a paraphrase of the score
/// math — only what [`SelectionRationale`] itself already says.
fn rationale_summary(rationale: &SelectionRationale) -> String {
    if let Some(summary) = rationale.display_summary() {
        return summary.to_owned();
    }
    let mut text = format!(
        "decisive on {}",
        rationale
            .decisive_components()
            .iter()
            .map(|component| format!("`{}`", component.as_str()))
            .collect::<Vec<_>>()
            .join(", ")
    );
    if rationale.tie_break_applied() {
        text.push_str("; tie-break applied");
    }
    if rationale.exploration_changed_winner() {
        text.push_str("; exploration changed the winner");
    }
    text
}

/// Renders a code list, or `none` when Cosmos reported nothing.
fn reason_list(reasons: &[String]) -> String {
    if reasons.is_empty() {
        return "none".to_string();
    }
    reasons
        .iter()
        .map(|reason| format!("`{reason}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The stable label for one intent.
const fn intent_label(intent: CodingIntent) -> &'static str {
    match intent {
        CodingIntent::Fast => "fast",
        CodingIntent::Careful => "careful",
        CodingIntent::Thorough => "thorough",
        // Unreachable through a single `/topology <spec>` submission — see
        // `parse_intent`'s caller there — but reachable through
        // `/topology compare`, which is the point of allowing it there.
        CodingIntent::Answer => "answer",
    }
}

/// Maps one spec's intent label to the closed [`CodingIntent`] vocabulary.
///
/// `None` for anything else, including labels this crate does not define —
/// callers decide which of the four labels their own grammar accepts.
fn parse_intent(label: &str) -> Option<CodingIntent> {
    match label {
        "fast" => Some(CodingIntent::Fast),
        "careful" => Some(CodingIntent::Careful),
        "thorough" => Some(CodingIntent::Thorough),
        "answer" => Some(CodingIntent::Answer),
        _ => None,
    }
}

/// A Cosmos stable name for work the user did not name, keyed by the target
/// file and the submission's ordinal so two submissions are two tasks.
///
/// `prefix` namespaces the reference by how the work was submitted —
/// `"user"` for an accepted single-edit task, `"compare"` for a comparison —
/// so a reference alone already says which kind of run produced it.
fn derive_reference(prefix: &str, file: &str, ordinal: u64) -> String {
    let mut slug = String::new();
    for byte in file.bytes().take(REFERENCE_SLUG_MAX) {
        let lowered = byte.to_ascii_lowercase();
        if lowered.is_ascii_lowercase() || lowered.is_ascii_digit() {
            slug.push(char::from(lowered));
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        format!("{prefix}/task/{ordinal}")
    } else {
        format!("{prefix}/{slug}/{ordinal}")
    }
}

/// Splits a spec into its `key=value` fields, honouring quoted values.
fn split_fields(spec: &str) -> Result<Vec<(String, String)>, String> {
    let mut fields = Vec::new();
    let mut chars = spec.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut key = String::new();
        loop {
            match chars.next() {
                Some('=') => break,
                Some(c) if c.is_whitespace() => {
                    return Err(format!(
                        "`{key}` has no `=value`; every field is written `key=value` \
                         (usage: /topology {SYNTAX})"
                    ));
                }
                Some(c) => key.push(c),
                None => {
                    return Err(format!(
                        "`{key}` has no `=value`; every field is written `key=value` \
                         (usage: /topology {SYNTAX})"
                    ));
                }
            }
        }
        if key.is_empty() {
            return Err("a field name is missing before `=`".to_string());
        }
        let value = read_value(&mut chars, &key)?;
        fields.push((key.to_ascii_lowercase(), value));
    }
    Ok(fields)
}

/// Reads one field value: bare up to the next space, or quoted with `\n`,
/// `\t`, `\r`, `\\` and escaped-quote escapes.
fn read_value(chars: &mut Peekable<Chars<'_>>, key: &str) -> Result<String, String> {
    let mut value = String::new();
    let quote = match chars.peek() {
        Some(&quote @ ('"' | '\'')) => {
            chars.next();
            quote
        }
        _ => {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                value.push(c);
                chars.next();
            }
            return Ok(value);
        }
    };
    loop {
        match chars.next() {
            None => {
                return Err(format!(
                    "field `{key}` opens a {quote} quote that is never closed"
                ));
            }
            Some(c) if c == quote => return Ok(value),
            Some('\\') => match chars.next() {
                Some('n') => value.push('\n'),
                Some('t') => value.push('\t'),
                Some('r') => value.push('\r'),
                Some('\\') => value.push('\\'),
                Some(c) if c == quote => value.push(c),
                Some(c) => {
                    return Err(format!(
                        "field `{key}` has an unknown escape `\\{c}`; \
                         use \\n, \\t, \\r, \\\\ or \\{quote}"
                    ));
                }
                None => return Err(format!("field `{key}` ends with a dangling backslash")),
            },
            Some(c) => value.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use polaris::system::server::Server;
    use polaris_cosmos::topology::{TopologyCatalog, TopologyCatalogPlugin};

    use super::*;
    use crate::agents::UnattendedGrants;
    use crate::cosmos::compare::ComparisonArm;
    use crate::cosmos::topology::CodingTopologyPackPlugin;

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp workspace");
        std::fs::create_dir_all(dir.path().join("src")).expect("src dir");
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 {\n    todo!()\n}\n",
        )
        .expect("seed lib.rs");
        dir
    }

    /// [`workspace`] plus a second stubbed file, for specs naming more than
    /// one edit or a scope spanning more than one path.
    fn workspace_with_second_file() -> tempfile::TempDir {
        let dir = workspace();
        std::fs::write(
            dir.path().join("src/util.rs"),
            "pub fn helper() -> i32 {\n    todo!()\n}\n",
        )
        .expect("seed util.rs");
        dir
    }

    /// The same sealed catalog `main.rs` reads back for `/topologies`,
    /// assembled once and cloned for every test — cheap, because sealing
    /// makes the snapshot an immutable, shareable value.
    fn sealed_catalog() -> TopologyCatalogSnapshot {
        static CATALOG: std::sync::OnceLock<TopologyCatalogSnapshot> = std::sync::OnceLock::new();
        CATALOG
            .get_or_init(|| {
                let mut server = Server::new();
                server.add_plugins(TopologyCatalogPlugin);
                server.add_plugins(CodingTopologyPackPlugin);
                // A fresh thread, so the first caller may itself be inside a runtime.
                let server = std::thread::spawn(move || {
                    tokio::runtime::Runtime::new()
                        .expect("a runtime for the test-only sealed catalog")
                        .block_on(server.finish())
                        .expect("the coding pack assembles");
                    server
                })
                .join()
                .expect("the sealed catalog thread completes");
                server
                    .get_global::<TopologyCatalog>()
                    .expect("the provider globalizes the catalog")
                    .snapshot()
                    .expect("the catalog is sealed")
            })
            .clone()
    }

    fn work(dir: &tempfile::TempDir, mode: PermissionMode) -> TopologyWork {
        TopologyWork::new(
            CodingTaskQueue::new(),
            dir.path(),
            sealed_catalog(),
            move || mode.clone(),
        )
    }

    #[test]
    fn a_quoted_spec_becomes_one_exact_coding_task() {
        let dir = workspace();
        let work = work(&dir, PermissionMode::AutoEdit);
        let prepared = work
            .prepare("file=src/lib.rs find=\"    todo!()\\n\" replace=\"    a + b\\n\" intent=fast")
            .expect("the spec is well formed");
        assert!(!prepared.needs_confirmation());
        let edit = prepared
            .task
            .edits()
            .first()
            .expect("exact work carries its stated edit");
        assert_eq!(edit.file(), "src/lib.rs");
        assert_eq!(edit.find(), "    todo!()\n");
        assert_eq!(edit.replace(), "    a + b\n");
        assert_eq!(prepared.task.stub_marker(), Some(DEFAULT_STUB_MARKER));
        assert_eq!(prepared.task.intent(), CodingIntent::Fast);
        assert_eq!(prepared.reference(), "user/src-lib-rs/1");

        let accepted = work.accept(prepared);
        assert_eq!(accepted.queued, 1);
        assert_eq!(accepted.intent, "fast");
        assert!(accepted.summary().contains("user/src-lib-rs/1"));
    }

    #[test]
    fn a_quoted_statement_becomes_general_coding_work() {
        let dir = workspace();
        let work = work(&dir, PermissionMode::AutoEdit);
        let prepared = work
            .prepare(
                "statement=\"where is fn add\\nplease show me\" ref=question \
                 intent=answer topology=code.tree-question",
            )
            .expect("the statement form is well formed");
        assert!(prepared.task.edits().is_empty());
        assert_eq!(prepared.task.statement(), "where is fn add\nplease show me");
        assert_eq!(prepared.task.intent(), CodingIntent::Answer);
        assert!(prepared.task.question().is_answerable());
    }

    #[test]
    fn malformed_and_unreachable_specs_are_refused_with_the_field_named() {
        let dir = workspace();
        let work = work(&dir, PermissionMode::AutoEdit);
        for (spec, expected) in [
            (
                "file=src/lib.rs find=x",
                "missing required field(s) replace",
            ),
            ("file", "has no `=value`"),
            (
                "file=src/lib.rs find=x replace=y depth=3",
                "`depth` is not a field",
            ),
            ("file=src/lib.rs find=\"x replace=y", "never closed"),
            (
                "file=src/lib.rs find=x replace=y intent=quick",
                "is not one of fast",
            ),
            (
                "file=src/lib.rs find=x replace=y file=other.rs",
                "was given twice",
            ),
            ("file=src/lib.rs find= replace=y", "find must not be empty"),
        ] {
            let refused = work.prepare(spec).expect_err(spec);
            assert!(
                matches!(refused, WorkRefused::Malformed(_)),
                "{spec} -> {refused:?}"
            );
            assert!(
                refused.detail().contains(expected),
                "{spec} -> {}",
                refused.detail()
            );
        }

        let escaping = work
            .prepare("file=../outside.rs find=x replace=y")
            .expect_err("the sandbox is enforced at acceptance");
        assert!(matches!(escaping, WorkRefused::Rejected(_)));
        let missing = work
            .prepare("file=src/nope.rs find=x replace=y")
            .expect_err("a file that is not there cannot be edited");
        assert!(missing.detail().contains("is not a file"), "{missing}");
        let noop = work
            .prepare("file=src/lib.rs find=same replace=same")
            .expect_err("a no-op edit is not work");
        assert!(matches!(noop, WorkRefused::Rejected(_)), "{noop:?}");
    }

    #[test]
    fn numbered_edit_fields_accept_several_exact_edits() {
        let dir = workspace_with_second_file();
        let work = work(&dir, PermissionMode::AutoEdit);
        let prepared = work
            .prepare(
                "file=src/lib.rs find=todo!() replace=0 \
                 file2=src/util.rs find2=todo!() replace2=1",
            )
            .expect("two well-formed edits");
        let edits = prepared.task.edits();
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].file(), "src/lib.rs");
        assert_eq!(edits[1].file(), "src/util.rs");
        assert_eq!(edits[1].find(), "todo!()");
        assert_eq!(edits[1].replace(), "1");
        assert!(prepared.preview().contains("across 2 edits"));

        let accepted = work.accept(prepared);
        assert_eq!(accepted.edit_count, 2);
        assert!(accepted.summary().contains("across 2 edits"));
    }

    #[test]
    fn a_partial_numbered_edit_group_is_refused() {
        let dir = workspace_with_second_file();
        let work = work(&dir, PermissionMode::AutoEdit);
        let refused = work
            .prepare("file=src/lib.rs find=todo!() replace=0 file2=src/util.rs find2=todo!()")
            .expect_err("edit 2 is missing replace2");
        assert!(matches!(refused, WorkRefused::Malformed(_)));
        assert!(
            refused
                .detail()
                .contains("edit 2 is missing field(s) replace"),
            "{}",
            refused.detail()
        );
    }

    #[test]
    fn a_numbered_edit_with_an_empty_find_is_refused() {
        let dir = workspace_with_second_file();
        let work = work(&dir, PermissionMode::AutoEdit);
        let refused = work
            .prepare("file=src/lib.rs find=todo!() replace=0 file2=src/util.rs find2= replace2=1")
            .expect_err("edit 2's find is empty");
        assert!(matches!(refused, WorkRefused::Malformed(_)));
        assert!(
            refused.detail().contains("edit 2: find must not be empty"),
            "{}",
            refused.detail()
        );
    }

    #[test]
    fn an_edit_index_beyond_the_maximum_is_an_unknown_field() {
        let dir = workspace();
        let work = work(&dir, PermissionMode::AutoEdit);
        let refused = work
            .prepare(
                "file=src/lib.rs find=todo!() replace=0 \
                 file9=src/lib.rs find9=x replace9=y",
            )
            .expect_err("index 9 is beyond MAX_EDITS");
        assert!(matches!(refused, WorkRefused::Malformed(_)));
        assert!(
            refused.detail().contains("`file9` is not a field"),
            "{}",
            refused.detail()
        );
    }

    #[test]
    fn scope_accepts_a_comma_separated_list_of_workspace_paths() {
        let dir = workspace_with_second_file();
        let work = work(&dir, PermissionMode::AutoEdit);
        let prepared = work
            .prepare("file=src/lib.rs find=todo!() replace=0 scope=src/lib.rs,src/util.rs")
            .expect("both scope paths are inside the workspace");
        assert_eq!(
            prepared.task.scope(),
            ["src/lib.rs".to_owned(), "src/util.rs".to_owned()]
        );
    }

    #[test]
    fn scope_does_not_require_the_path_to_already_exist() {
        let dir = workspace();
        let work = work(&dir, PermissionMode::AutoEdit);
        let prepared = work
            .prepare("file=src/lib.rs find=todo!() replace=0 scope=src/not-yet-created.rs")
            .expect("a scope belief does not require the path to exist yet");
        assert_eq!(prepared.task.scope(), ["src/not-yet-created.rs".to_owned()]);
    }

    #[test]
    fn a_scope_entry_that_escapes_the_workspace_is_refused() {
        let dir = workspace();
        let work = work(&dir, PermissionMode::AutoEdit);
        let refused = work
            .prepare("file=src/lib.rs find=todo!() replace=0 scope=../outside.rs")
            .expect_err("a scope entry outside the workspace is refused");
        assert!(matches!(refused, WorkRefused::Rejected(_)));
        assert!(
            refused.detail().contains("escapes the workspace"),
            "{}",
            refused.detail()
        );
    }

    #[test]
    fn a_stated_command_becomes_the_task_success_check() {
        let dir = workspace();
        let work = work(&dir, PermissionMode::AutoEdit);
        let prepared = work
            .prepare(
                "file=src/lib.rs find=todo!() replace=0 command=cargo \
                 command-args=test,--quiet",
            )
            .expect("a command success check is well formed");
        match prepared.task.success() {
            SuccessCheck::Command(command) => {
                assert_eq!(command.program(), "cargo");
                assert_eq!(
                    command.args().to_vec(),
                    vec!["test".to_owned(), "--quiet".to_owned()]
                );
            }
            other => panic!("expected a command success check, got {other:?}"),
        }
    }

    #[test]
    fn a_stated_command_works_on_the_statement_form_too() {
        let dir = workspace();
        let work = work(&dir, PermissionMode::AutoEdit);
        let prepared = work
            .prepare("statement=\"fix the bug\" command=cargo command-args=test")
            .expect("a command success check is well formed on the statement form too");
        assert!(matches!(prepared.task.success(), SuccessCheck::Command(_)));
    }

    #[test]
    fn command_args_without_command_is_refused() {
        let dir = workspace();
        let work = work(&dir, PermissionMode::AutoEdit);
        let refused = work
            .prepare("file=src/lib.rs find=todo!() replace=0 command-args=test")
            .expect_err("command-args requires command");
        assert!(matches!(refused, WorkRefused::Malformed(_)));
        assert!(
            refused
                .detail()
                .contains("`command-args` requires `command`"),
            "{}",
            refused.detail()
        );
    }

    #[test]
    fn stub_and_command_are_mutually_exclusive() {
        let dir = workspace();
        let work = work(&dir, PermissionMode::AutoEdit);
        let refused = work
            .prepare("file=src/lib.rs find=todo!() replace=0 stub=todo!() command=true")
            .expect_err("stub and command cannot both be given");
        assert!(matches!(refused, WorkRefused::Malformed(_)));
        assert!(
            refused.detail().contains("alternative success checks"),
            "{}",
            refused.detail()
        );
    }

    #[test]
    fn statement_scope_and_stub_together_form_a_survey_task() {
        let dir = workspace_with_second_file();
        let work = work(&dir, PermissionMode::AutoEdit);
        let prepared = work
            .prepare(
                "statement=\"check for leftover stubs\" scope=src/lib.rs,src/util.rs \
                 stub=todo!()",
            )
            .expect("a survey is a statement with scope and a stub marker, no edits");
        assert!(prepared.task.edits().is_empty());
        assert_eq!(prepared.task.scope().len(), 2);
        assert_eq!(prepared.task.stub_marker(), Some("todo!()"));
        assert!(
            prepared.task.is_survey(),
            "this exact combination is what is_survey detects"
        );
    }

    #[test]
    fn an_empty_stub_on_the_statement_form_is_silently_ignored() {
        let dir = workspace();
        let work = work(&dir, PermissionMode::AutoEdit);
        let prepared = work
            .prepare("statement=\"answer a question\" stub=\"\"")
            .expect("an empty stub is treated as not given");
        assert_eq!(prepared.task.stub_marker(), None);
    }

    #[test]
    fn the_posture_decides_whether_a_submission_is_queued() {
        let dir = workspace();
        let spec = "file=src/lib.rs find=todo!() replace=0";

        let auto = work(&dir, PermissionMode::AutoEdit);
        assert!(!auto.prepare(spec).expect("allowed").needs_confirmation());

        let confirm = work(&dir, PermissionMode::Confirm);
        assert!(
            confirm
                .prepare(spec)
                .expect("confirmable")
                .needs_confirmation(),
            "the interactive default owes a confirmation before anything is queued"
        );

        for denied in [
            PermissionMode::Plan,
            PermissionMode::Unattended(UnattendedGrants::default()),
        ] {
            let work = work(&dir, denied.clone());
            let refused = work.prepare(spec).expect_err("a denied edit is refused");
            assert!(matches!(refused, WorkRefused::Posture(_)), "{denied:?}");
            assert_eq!(work.queued(), 0, "a refused submission queues nothing");
            assert!(
                work.accept_spec(spec).is_err(),
                "the launch flag is refused by the same gate"
            );
        }

        let unattended = work(
            &dir,
            PermissionMode::Unattended(UnattendedGrants {
                edit_file: true,
                run_command_patterns: vec![],
            }),
        );
        let accepted = unattended.accept_spec(spec).expect("granted edits queue");
        assert_eq!(accepted.queued, 1);
    }

    #[tokio::test]
    async fn confirm_mode_queues_only_after_an_explicit_confirmation() {
        use polaris_tui::commands::CommandInvocation;

        let dir = workspace();
        let work = work(&dir, PermissionMode::Confirm);
        let registry = CommandRegistry::with_builtins();
        work.register(&registry).expect("/topology registers");

        let proposal = registry
            .dispatch(&CommandInvocation::new(
                "topology",
                "file=src/lib.rs find=todo!() replace=0",
            ))
            .await;
        let CommandOutcome::Reply(preview) = &proposal else {
            panic!("a confirm-mode submission asks first, got {proposal:?}");
        };
        assert!(preview.contains("/topology confirm"), "{preview}");
        assert!(preview.contains("todo!()"), "{preview}");
        assert!(!proposal.ends_turn(), "asking does not end the turn");
        assert_eq!(work.queued(), 0, "nothing is queued before confirmation");

        let accepted = registry
            .dispatch(&CommandInvocation::new("topology", "confirm"))
            .await;
        let CommandOutcome::Control(receipt) = &accepted else {
            panic!("a confirmation queues the work, got {accepted:?}");
        };
        assert!(receipt.contains("Accepted coding work"), "{receipt}");
        assert!(
            accepted.ends_turn(),
            "the idle turn ends so the next one reaches the selection point"
        );
        assert_eq!(work.queued(), 1);

        let replayed = registry
            .dispatch(&CommandInvocation::new("topology", "confirm"))
            .await;
        assert!(
            matches!(replayed, CommandOutcome::Error(_)),
            "a confirmation cannot be replayed into a second accepted task: {replayed:?}"
        );
        assert_eq!(work.queued(), 1);
    }

    #[tokio::test]
    async fn cancel_drops_the_proposal_and_status_teaches_the_syntax() {
        use polaris_tui::commands::CommandInvocation;

        let dir = workspace();
        let work = work(&dir, PermissionMode::Confirm);
        let registry = CommandRegistry::with_builtins();
        work.register(&registry).expect("/topology registers");

        let CommandOutcome::Reply(status) = registry
            .dispatch(&CommandInvocation::new("topology", ""))
            .await
        else {
            panic!("bare /topology reports status");
        };
        assert!(
            status.contains("Topology work queued for the next turn: 0"),
            "{status}"
        );
        assert!(status.contains("file=<path>"), "{status}");

        let _ = registry
            .dispatch(&CommandInvocation::new(
                "topology",
                "file=src/lib.rs find=todo!() replace=0",
            ))
            .await;
        let cancelled = registry
            .dispatch(&CommandInvocation::new("topology", "cancel"))
            .await;
        assert!(
            matches!(cancelled, CommandOutcome::Reply(_)),
            "{cancelled:?}"
        );
        assert_eq!(work.queued(), 0);
        let again = registry
            .dispatch(&CommandInvocation::new("topology", "cancel"))
            .await;
        assert!(matches!(again, CommandOutcome::Error(_)), "{again:?}");
    }

    #[test]
    fn compare_specs_are_refused_the_same_way_a_single_edit_spec_is() {
        let dir = workspace();
        let work = work(&dir, PermissionMode::AutoEdit);
        for (spec, expected) in [
            (
                "file=src/lib.rs find=x replace=y",
                "missing required field(s) intents",
            ),
            (
                "intents=fast file=src/lib.rs find=x replace=y",
                "must name exactly two",
            ),
            (
                "intents=fast,quick file=src/lib.rs find=x replace=y",
                "is not one of fast, careful, thorough or answer",
            ),
            (
                "intents=fast,careful file=src/lib.rs find=x replace=y depth=3",
                "`depth` is not a field of a comparison",
            ),
            (
                "intents=fast,careful file=src/lib.rs find= replace=y",
                "find must not be empty",
            ),
        ] {
            let refused = work.parse_compare_spec(spec).expect_err(spec);
            assert!(
                matches!(refused, WorkRefused::Malformed(_)),
                "{spec} -> {refused:?}"
            );
            assert!(
                refused.detail().contains(expected),
                "{spec} -> {}",
                refused.detail()
            );
        }
    }

    #[test]
    fn a_compare_spec_accepts_answer_where_a_single_edit_spec_would_refuse_it() {
        let dir = workspace();
        let work = work(&dir, PermissionMode::AutoEdit);
        let parsed = work
            .parse_compare_spec("intents=careful,answer file=src/lib.rs find=todo!() replace=0")
            .expect("answer is a valid compare intent");
        assert_eq!(
            parsed.intents,
            [CodingIntent::Careful, CodingIntent::Answer]
        );
        assert!(parsed.reference.starts_with("compare/src-lib-rs/"));
    }

    #[tokio::test]
    async fn compare_reports_two_real_and_different_outcomes_and_never_touches_the_real_workspace()
    {
        use polaris_tui::commands::CommandInvocation;

        let dir = workspace();
        let before = std::fs::read(dir.path().join("src/lib.rs")).expect("seed file");
        let work = work(&dir, PermissionMode::Confirm);
        let registry = CommandRegistry::with_builtins();
        work.register(&registry).expect("/topology registers");

        // `careful` and `answer` share every hard constraint except one:
        // `answer` forbids the source-mutation capability, so it cannot land
        // on an edit-shaped registration — but `code.tree-question` (which
        // answers a bounded question and writes nothing) is still a real
        // candidate for it, so this pair reliably produces two *different*
        // real conclusions for the same edit, not one refused and one
        // executed. That is itself the comparison this affordance exists to
        // show.
        let outcome = registry
            .dispatch(&CommandInvocation::new(
                "topology",
                "compare intents=careful,answer file=src/lib.rs find=todo!() replace=0",
            ))
            .await;
        let CommandOutcome::Reply(report) = &outcome else {
            panic!("a comparison reports without ending the turn, got {outcome:?}");
        };
        assert!(!outcome.ends_turn(), "a comparison is not queued work");
        assert!(report.contains("Arm 1 — intent `careful`"), "{report}");
        assert!(report.contains("Arm 2 — intent `answer`"), "{report}");
        assert!(
            report.contains("A topology ran") && report.contains("resolved the coding task"),
            "the careful arm should resolve the edit: {report}"
        );
        assert!(
            report.matches("A topology ran").count() == 2,
            "both arms should reach a real topology, just different ones: {report}"
        );
        assert!(
            report.contains("escalated the coding task"),
            "the answer arm should decline to resolve an edit on points, not refuse to run \
             at all: {report}"
        );

        // Nothing was queued, and the confirm-mode posture never had to be
        // asked: a comparison cannot edit the live workspace at all.
        assert_eq!(work.queued(), 0);
        let after = std::fs::read(dir.path().join("src/lib.rs")).expect("seed file");
        assert_eq!(before, after, "the real workspace must be untouched");
    }

    /// Also the no-execution-without-admission proof: `NoEligibleCandidate`
    /// is the mechanical admission boundary refusing both arms *before*
    /// either graph would start, and the file-byte assertion at the end
    /// confirms nothing ran anyway.
    #[tokio::test]
    async fn a_verifier_capability_neither_arm_can_supply_refuses_both_arms_and_reports_both() {
        use polaris_tui::commands::CommandInvocation;

        let dir = workspace();
        let before = std::fs::read(dir.path().join("src/lib.rs")).expect("seed file");
        let work = work(&dir, PermissionMode::Confirm);
        let registry = CommandRegistry::with_builtins();
        work.register(&registry).expect("/topology registers");

        // No coding topology in this embodiment offers a comparative
        // verifier (see `CodingObjective::requiring_comparative_verifier`'s
        // own doc), so requiring one is the one hard constraint that is
        // genuinely, reliably unsatisfiable — real refusal, not a fabricated
        // one, and uniform across intents rather than a difference between
        // them. This is the honest shape "refused arm still reports" takes
        // here: both arms refused, both still named and reported, neither
        // aborting the other.
        let outcome = registry
            .dispatch(&CommandInvocation::new(
                "topology",
                "compare intents=fast,careful verifier=comparative file=src/lib.rs \
                 find=todo!() replace=0",
            ))
            .await;
        let CommandOutcome::Reply(report) = &outcome else {
            panic!("a comparison reports without ending the turn, got {outcome:?}");
        };
        assert!(!outcome.ends_turn());
        assert!(report.contains("Arm 1 — intent `fast`"), "{report}");
        assert!(report.contains("Arm 2 — intent `careful`"), "{report}");
        assert_eq!(
            report.matches("No topology was eligible").count(),
            2,
            "both arms should be refused, and both reported, not just one: {report}"
        );

        assert_eq!(work.queued(), 0, "a refused comparison queues nothing");
        let after = std::fs::read(dir.path().join("src/lib.rs")).expect("seed file");
        assert_eq!(
            before, after,
            "nothing was ever admitted, so nothing edited"
        );
    }

    #[test]
    fn render_comparison_reports_every_arm_regardless_of_how_it_ended() {
        // `compare_intents` cannot honestly make one arm of a same-task,
        // same-tree comparison land on `Selected` while the other fails
        // setup or selection outright — every hard constraint this
        // embodiment's `CodingObjective` derives from an intent is either
        // identical across Fast/Careful/Thorough, or (for `Answer`) still
        // satisfiable by a real registration (see the two tests above).
        // So the rendering guarantee this test protects — a fatally-refused
        // arm never suppresses the other arm's own reported outcome — is
        // exercised here directly on the report values rather than through a
        // real run neither can trigger honestly.
        let report = ComparisonReport {
            reference: "compare/probe/1".to_string(),
            source_tree: PathBuf::from("/tmp/does-not-matter"),
            arms: [
                ComparisonArm {
                    intent: CodingIntent::Fast,
                    outcome: ArmOutcome::SetupFailed(
                        "the isolated workspace could not be created: disk full".to_string(),
                    ),
                },
                ComparisonArm {
                    intent: CodingIntent::Careful,
                    outcome: ArmOutcome::SelectionFailed(
                        crate::cosmos::host::TopologySelectionError::Selection(
                            "simulated selection failure".to_string(),
                        ),
                    ),
                },
            ],
        };
        let rendered = render_comparison(&report);
        assert!(rendered.contains("Arm 1 — intent `fast`"), "{rendered}");
        assert!(rendered.contains("disk full"), "{rendered}");
        assert!(rendered.contains("Arm 2 — intent `careful`"), "{rendered}");
        assert!(
            rendered.contains("simulated selection failure"),
            "{rendered}"
        );
    }

    #[test]
    fn derived_references_are_valid_cosmos_stable_names() {
        for file in ["src/lib.rs", "Deep/Path/MOD.rs", "...", "a"] {
            let reference = derive_reference("user", file, 7);
            let task = CodingTask::new(
                reference.clone(),
                std::env::current_dir().expect("cwd"),
                file,
                "a",
                "b",
                DEFAULT_STUB_MARKER,
                CodingIntent::Fast,
            );
            assert!(task.is_ok(), "{reference} was rejected for {file}");
        }
    }
}
