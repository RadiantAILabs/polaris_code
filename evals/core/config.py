"""The whole recorded configuration of a sweep, minus anything a dataset owns.

Every run-wide number lives here as a constant or a field of [`RunConfig`][],
never as prose in a document or a literal buried in the orchestrator. A new
full sweep is therefore a change of *arguments*, not a change of code.

The two that matter most:

* `MODES` — the per-mode matrix. The denominator is *not* here: it belongs to
  whichever dataset pack is being run, and the pack's suite reports it.
* `EVIDENCE_BAR` — the three-sweep credibility rule. A conclusion that does not
  clear it is reported as ``not distinguished at this budget``.

Dataset constants live in ``evals.packs.*``, not here: the suite size, the
dataset-specific cost ceiling and the layout of a dataset's images.

The conclusion vocabulary lives here too, with the bar it answers to, and so
does :func:`not_evidence_label` — the single rule deciding whether a run may be
presented as a measurement at all. Four artefacts have to agree about that
answer (the manifest, the summary, every cell and the command's own output), and
deriving it four times is how three of them end up disagreeing.
"""

from __future__ import annotations

import dataclasses
from typing import Mapping, Sequence

from .process import REPLAY

#: The conclusion of a run that cleared the evidence bar on some pair.
CONCLUSION_CREDIBLE = "credible difference"

#: The conclusion of a complete run where no pair cleared the bar.
CONCLUSION_NOT_DISTINGUISHED = "not distinguished at this budget"

#: The conclusion of a run that did not complete every cell.
CONCLUSION_INCOMPLETE = "incomplete"

#: The conclusion of a run whose episodes were replayed rather than executed.
CONCLUSION_PLUMBING = "plumbing-demonstration"

#: The conclusion of a run configured from a named iteration profile. The label
#: spells out what it is rather than naming a category a reader would have to
#: look up, because it is going to be pasted somewhere without its context.
CONCLUSION_ITERATION = "iteration-signal-not-evidence"

#: Schema of the manifest the harness writes.
MANIFEST_SCHEMA = "polaris-code-harness-eval/v1"

#: Schema of the episode records the harness appends.
EPISODE_SCHEMA = "polaris-code-harness-eval-episode/v1"

#: Schema of the derived summary the harness collates.
SUMMARY_SCHEMA = "polaris-code-harness-eval-summary/v1"

#: The graph-search modes that form the matrix, in their canonical order.
#: These are exactly the values ``POLARIS_CODE_GRAPH_SEARCH`` accepts. They are
#: a property of the *agent under test*, not of any dataset, which is why they
#: stayed in the core: every pack runs the same three arms.
MODES: tuple[str, ...] = ("curated", "freeform", "both")

#: ``RunConfig.axis`` value for the tool-exposure matrix: ``modes`` holds a
#: subsequence of :data:`MODES`, propagated through ``POLARIS_CODE_GRAPH_SEARCH``.
AXIS_GRAPH_SEARCH = "graph-search"

#: ``RunConfig.axis`` value for a topology sweep: ``modes`` holds registered
#: topology ids, propagated through ``POLARIS_CODE_TOPOLOGY_PIN``. The ids
#: below are the sealed catalog assembled by this build. See
#: ``docs/harness-eval.md`` for why this axis does not cross the graph-search
#: one in the same sweep.
AXIS_TOPOLOGY = "topology"

#: ``RunConfig.axis`` value for a composition sweep: ``modes`` holds
#: composition-policy identities, propagated through
#: ``POLARIS_CODE_COMPOSITION_POLICY``. Topology selection is left automatic
#: (no ``POLARIS_CODE_TOPOLOGY_PIN`` is set) so the same registration is
#: chosen for a given instance regardless of which policy assembles its
#: recipe — the policy is the only thing that varies. ``graph_search_mode``
#: holds the tool-exposure surface fixed, exactly as it does for
#: :data:`AXIS_TOPOLOGY`. See ``docs/harness-eval.md`` for why this axis does
#: not cross the graph-search or topology ones in the same sweep.
AXIS_COMPOSITION = "composition"

#: Every experiment axis a run may be configured against, in the order the CLI
#: documents them.
EXPERIMENT_AXES: tuple[str, ...] = (AXIS_GRAPH_SEARCH, AXIS_TOPOLOGY, AXIS_COMPOSITION)

#: The registered topology identities in this build's sealed catalog, in
#: catalog order. A topology sweep is deliberately explicit rather than
#: defaulting to every registration, because the caller must choose both the
#: cells to compare and an appropriate cost ceiling.
TOPOLOGIES: tuple[str, ...] = (
    "code.direct-edit",
    "code.prediction-error-edit",
    "code.worklist-sweep-edit",
    "code.test-gated-edit",
    "code.model-proposed-edit",
    "code.coverage-audit",
    "code.tree-question",
)

#: The environment variable that selects the graph-search surface.
GRAPH_SEARCH_VAR = "POLARIS_CODE_GRAPH_SEARCH"

#: The explicitly selected composition policy for an episode.
COMPOSITION_POLICY_VAR = "POLARIS_CODE_COMPOSITION_POLICY"

#: The gate planner used when the composition policy is ``gates``.
GATE_PLANNER_VAR = "POLARIS_CODE_GATE_PLANNER"

#: How many times the agent loop may go round in one turn.
MAX_TURNS_VAR = "POLARIS_CODE_MAX_TURNS"

#: The ``max_tokens`` every model request carries.
MAX_OUTPUT_TOKENS_VAR = "POLARIS_CODE_MAX_OUTPUT_TOKENS"

#: The binary's own defaults for the two limits above
#: (``DEFAULT_MAX_TURNS`` in ``crates/polaris_code/src/agents/code_agent.rs``
#: and ``DEFAULT_MAX_OUTPUT_TOKENS`` in ``crates/polaris_code/src/main.rs``). The harness always
#: passes both, so a run's limits are in its manifest rather than implied.
DEFAULT_MAX_TURNS = 100
DEFAULT_MAX_OUTPUT_TOKENS = 16384

#: The composition policies this build's runtime-assembly host accepts, in
#: canonical order. Mirrors :data:`TOPOLOGIES`: a composition sweep names the
#: policies it compares explicitly rather than defaulting to every one.
COMPOSITION_POLICIES: tuple[str, ...] = (
    "deterministic",
    "model",
    "gates",
    "gates-model",
)

#: The environment variable that selects the model.
MODEL_VAR = "POLARIS_CODE_MODEL"

#: The environment variable carrying the provider key into the environment.
API_KEY_VAR = "ANTHROPIC_API_KEY"

#: Domains supported by the domain-plan evaluation, in canonical
#: order. Mirrors `cosmos::domains::Domain` in the Rust crate.
DOMAINS: tuple[str, ...] = ("none", "django", "sympy", "sphinx")

#: The plan/notes treatment the same experiment selects, in canonical order.
#: Mirrors `cosmos::domains::Arm`. ``"loop"`` runs one generic loop and,
#: unlike the domain-specific arms, reads no domain plan or notes, so it pairs
#: only with domain ``"none"``.
#: ``"composed"`` is a model-built plan informed by the selected domain.
#: ``"fine"`` is likewise domain independent.
ARMS: tuple[str, ...] = ("generic", "generic-notes", "expert", "experience", "loop", "lean", "composed", "fine", "fine-ctx", "expert2")

#: The environment variable that selects the domain above.
DOMAIN_VAR = "POLARIS_CODE_DOMAIN"

#: The environment variable that selects the arm above.
ARM_VAR = "POLARIS_CODE_ARM"

#: The environment variable that pins one registered topology for a run,
#: read once at binary startup exactly like ``POLARIS_CODE_GRAPH_SEARCH``.
#: The selected identity is part of the recorded configuration a topology
#: sweep needs.
TOPOLOGY_VAR = "POLARIS_CODE_TOPOLOGY_PIN"

#: Environment variable *names* an episode is allowed to receive. Values are
#: never recorded anywhere in a run directory.
ALLOWED_ENV_NAMES: tuple[str, ...] = (
    API_KEY_VAR,
    MODEL_VAR,
    GRAPH_SEARCH_VAR,
    TOPOLOGY_VAR,
    COMPOSITION_POLICY_VAR,
    GATE_PLANNER_VAR,
    MAX_TURNS_VAR,
    MAX_OUTPUT_TOKENS_VAR,
    DOMAIN_VAR,
    ARM_VAR,
)

#: Prefix of the `lean` arm's own feature switches
#: (`POLARIS_CODE_LEAN_TRIM`, `POLARIS_CODE_LEAN_TWM`, and so on — see
#: `docs/gate-planning.md`). Unlike :data:`ALLOWED_ENV_NAMES` above, this is
#: not a closed, enumerated set: a new lean switch must reach a container
#: without the harness naming it in advance, so any host environment
#: variable starting with this prefix is forwarded by name.
LEAN_ENV_PREFIX = "POLARIS_CODE_LEAN_"

#: Prefix of experiment switches that apply to any arm (for example
#: ``POLARIS_CODE_EXP_SUBMIT_GUARD``). Forwarded by name like the lean ones.
EXP_ENV_PREFIX = "POLARIS_CODE_EXP_"

#: The lean arm's per-task estimated dollar ceiling. Not itself under
#: :data:`LEAN_ENV_PREFIX` (it predates the prefix and applies to more than
#: just lean in principle), so it is named explicitly alongside it.
MAX_COST_VAR = "POLARIS_CODE_MAX_COST_USD"


#: Provider selection a Bedrock run needs inside the container, forwarded by
#: name whenever the host sets it. ``POLARIS_CODE_MODELS`` declares the model
#: set the binary may select from; ``AWS_REGION`` pins the Bedrock region.
PROVIDER_ENV_NAMES: tuple[str, ...] = (
    "POLARIS_CODE_PROVIDER",
    "POLARIS_CODE_MODELS",
    "AWS_REGION",
    # Run the agent inside the image's project environment (harness) and keep
    # that runtime in its shell tools (binary). Off unless set to 1.
    "POLARIS_CODE_TESTENV",
)

#: AWS credentials for ``POLARIS_CODE_PROVIDER=bedrock``. The host resolves
#: credentials through the standard AWS credential chain and each episode
#: receives them. Like :data:`API_KEY_VAR`, they travel by name only
#: (``--env NAME``) and never appear in a record or a process listing.
SECRET_ENV_NAMES: tuple[str, ...] = (
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
)


def lean_passthrough_names(environ: Mapping[str, str]) -> tuple[str, ...]:
    """Names of any lean switch or :data:`MAX_COST_VAR` set in `environ`.

    Sorted for a deterministic, reproducible manifest. This reads only
    *names* — the caller is responsible for reading each name's value from
    the live process environment separately, per episode, exactly as it
    already does for :data:`API_KEY_VAR`; no value is ever captured here or
    stored on a :class:`RunConfig`.
    """
    return tuple(
        sorted(
            name
            for name in environ
            if name == MAX_COST_VAR
            or name.startswith(LEAN_ENV_PREFIX)
            or name.startswith(EXP_ENV_PREFIX)
            or name in PROVIDER_ENV_NAMES
            or name in SECRET_ENV_NAMES
        )
    )

#: Where the harness installs the ``polaris-code`` binary inside a sandbox.
#: Ours, not a dataset's: the path we choose to put our own binary at.
CONTAINER_BINARY_PATH = "/opt/polaris/bin/polaris"

#: Repository-relative location of the static musl artifact built by
#: ``scripts/build-portable-binary.sh``. Benchmark images vary in their glibc
#: version, so this is the harness default instead of the host-native release
#: path.
PORTABLE_BINARY_PATH = "target/x86_64-unknown-linux-musl/release/polaris"

#: Conservative planning cost of one episode, in USD. It is used *only* to
#: project spend before a run starts; it is never substituted for measured
#: usage afterwards. A pack whose problems are materially larger or smaller
#: may override it on the guard.
PLANNING_COST_PER_EPISODE_USD = 0.84

#: How many real per-turn composition-chooser model calls the host will make
#: before it stops trying further fallback candidates and refuses
#: (``MAX_MODEL_CHOOSER_ATTEMPTS_PER_TURN`` in
#: ``crates/polaris_code/src/cosmos/composition.rs``). Not read from the
#: Rust crate — this package has no FFI into it — so the two are kept in sync
#: by hand; a change to the host's own constant must update this one too.
MAX_MODEL_CHOOSER_ATTEMPTS_PER_TURN = 3

#: Analytic (not measured — no chooser call has been billed to derive this)
#: worst-case cost of one real composition-chooser model call: its response
#: capped at the binary's default output-token limit
#: (:data:`DEFAULT_MAX_OUTPUT_TOKENS`), billed
#: at Sonnet's published per-token rate (`usage.rs`'s own pricing table:
#: $3/M input, $15/M output), plus a generous fixed 300-token estimate for
#: the chooser's own small, fixed-shape prompt. This is a bound derived from
#: real, cited constants, not a guess at what a call happens to cost.
COMPOSITION_CHOOSER_CALL_CEILING_USD = (300 * 3.0 + DEFAULT_MAX_OUTPUT_TOKENS * 15.0) / 1_000_000

#: Per-episode planning cost under each composition policy, for the
#: composition axis's own pre-run projection. ``deterministic`` never calls a
#: model to compose (`composition::choose` returns before any registry
#: lookup for that policy), so its rate is the ordinary planning cost
#: unchanged; ``model`` adds the host's own bounded worst case for chooser
#: calls on top, so the projection cannot under-count what a ``model`` cell
#: can spend the way one flat rate for both cells would.
COMPOSITION_POLICY_PLANNING_COST_PER_EPISODE_USD: dict[str, float] = {
    "deterministic": PLANNING_COST_PER_EPISODE_USD,
    "model": PLANNING_COST_PER_EPISODE_USD
    + MAX_MODEL_CHOOSER_ATTEMPTS_PER_TURN * COMPOSITION_CHOOSER_CALL_CEILING_USD,
    "gates": PLANNING_COST_PER_EPISODE_USD,
    "gates-model": PLANNING_COST_PER_EPISODE_USD
    + MAX_MODEL_CHOOSER_ATTEMPTS_PER_TURN * COMPOSITION_CHOOSER_CALL_CEILING_USD,
}

#: Default per-episode inference timeout, in seconds.
DEFAULT_EPISODE_TIMEOUT_SECONDS = 1800

#: Default per-cell grading timeout, in seconds.
DEFAULT_GRADE_TIMEOUT_SECONDS = 1800

#: Default worker count for both inference and grading.
DEFAULT_MAX_WORKERS = 4

#: How many times one confirmed *infrastructure* failure is retried with the
#: same configuration. Both attempts are retained; an agent failure, a timeout,
#: an empty patch and a non-applying patch are results, not infrastructure
#: failures, and are never retried.
INFRASTRUCTURE_RETRIES = 1


@dataclasses.dataclass(frozen=True)
class EvidenceBar:
    """The credibility rule a mode difference must clear to be called real.

    All four conditions must hold together. The harness records them and
    :mod:`evals.core.collate` reports
    ``not distinguished at this budget`` whenever any one fails.
    """

    #: Complete sweeps required before a difference may be called credible.
    min_sweeps: int = 3
    #: Minimum absolute resolved-rate difference, in percentage points.
    min_absolute_difference_points: float = 5.0
    #: Whether the sign of the difference must agree in every repetition.
    require_consistent_direction: bool = True
    #: Resamples in the paired task-cluster bootstrap.
    bootstrap_resamples: int = 10_000
    #: Two-sided confidence level whose interval must exclude zero.
    confidence: float = 0.95


#: The shared evidence bar. Constructed once so every caller and every recorded
#: manifest agrees on the same object; dataset packs cannot silently replace it
#: with a more favorable threshold.
EVIDENCE_BAR = EvidenceBar()


def not_evidence_label(
    *, process_runner: str | None, iteration_profile: str | None
) -> str | None:
    """The label a run that is not a measurement must carry, or ``None``.

    Both refusals are structural: they read the recorded configuration and
    ignore the arithmetic entirely, which is what makes them impossible to
    argue with after the fact.

    Replay wins when both apply. A replayed iteration run executed nothing, so
    its numbers are scripted fixtures; calling it an iteration signal would
    present them as real observations of a small sample, and that overstates
    what happened by more than the reverse understates it.
    """
    if process_runner == REPLAY:
        return CONCLUSION_PLUMBING
    if iteration_profile:
        return CONCLUSION_ITERATION
    return None


@dataclasses.dataclass(frozen=True)
class RunConfig:
    """Everything a run is reproducible from.

    Serialized verbatim into ``manifest.json``. ``recompute`` reads it back and
    must reach byte-identical conclusions from the same episodes and the same
    grader output, so no field here may be derived from wall-clock state.

    Three fields are consumed by the pack's providers rather than by the core:
    ``container_runtime`` and ``grade_timeout_seconds`` mean nothing to a pack
    that runs no sandbox and calls no external grader, and ``max_workers`` is
    handed to whatever grader the pack supplies. They live here because they are
    recorded configuration, and a run directory has exactly one manifest.

    ``modes`` and ``axis`` together name an experiment arm: on
    :data:`AXIS_GRAPH_SEARCH` ``modes`` holds a subsequence of :data:`MODES`;
    on :data:`AXIS_TOPOLOGY` it holds
    registered topology ids instead; on :data:`AXIS_COMPOSITION` it holds
    composition-policy identities instead. On the latter two,
    ``graph_search_mode`` names the single tool-exposure value held fixed
    across every one of that sweep's cells. Nothing downstream of
    configuration — scheduling, session/container identity, collation, the
    bootstrap — reads ``axis`` at all; it exists only so a manifest never
    leaves a reader to guess which vocabulary ``modes`` holds. See
    ``docs/harness-eval.md`` for why no two axes are ever swept in the same
    run.
    """

    #: The identifier of this run; also the run directory's name by convention.
    eval_run_id: str
    #: Stable name of the dataset pack this run evaluated, e.g.
    #: ``swebench-multilingual``. ``recompute`` cross-checks it when present.
    dataset_pack: str
    #: Model identity, as ``POLARIS_CODE_MODEL`` spells it.
    model: str
    #: The matrix: a subsequence of :data:`MODES` when ``axis`` is
    #: :data:`AXIS_GRAPH_SEARCH`, or of this build's registered topology ids
    #: when ``axis`` is :data:`AXIS_TOPOLOGY` — in canonical order either way.
    modes: tuple[str, ...]
    #: Complete sweeps to run. Three is the evidence bar; one is a baseline.
    repetitions: int
    #: Seed for the per-``(instance, repetition)`` mode-order shuffle.
    order_seed: int
    #: Seed for the bootstrap in :mod:`evals.core.statistics`.
    analysis_seed: int
    #: Instances to attempt. Empty means the whole pinned suite. A non-empty
    #: subset is a *plumbing* run: the denominator stays whatever the pack
    #: pins, so every cell it produces is incomplete and carries no score.
    instance_ids: tuple[str, ...]
    #: Per-episode inference timeout.
    episode_timeout_seconds: int
    #: Per-cell grading timeout.
    grade_timeout_seconds: int
    #: Parallel workers for inference and grading.
    max_workers: int
    #: The hard API-cost ceiling in USD. The guard refuses to *start* a run
    #: whose projection exceeds it and stops scheduling when spend reaches it.
    api_cost_ceiling_usd: float
    #: Path to the ``polaris-code`` binary the pack's environment installs.
    polaris_code_binary: str
    #: Sandbox runtime executable, e.g. ``docker`` or ``podman``.
    container_runtime: str
    #: Whether the unattended run may call ``edit_file``. Required for the
    #: agent to be able to produce a patch at all; see ``docs/harness-eval.md``.
    allow_edit: bool
    #: Shell-policy globs granted to the unattended run, possibly empty.
    allow_command: tuple[str, ...]
    #: Whether episodes submit their statement through the binary's existing
    #: topology-work affordance instead of starting a conversational turn.
    topology_work: bool
    #: ``subprocess`` for a measured run, ``replay`` for an offline plumbing
    #: demonstration. Recorded so a replayed directory can never be mistaken
    #: for a measured one.
    process_runner: str
    #: The evidence bar in force for this run.
    evidence_bar: EvidenceBar = EVIDENCE_BAR
    #: The named iteration profile this run was configured from, or ``None``
    #: for an ordinary run. Recorded so a fast directory is distinguishable
    #: from a measured one by reading its manifest alone, and so collation can
    #: refuse to score it whatever its numbers say.
    iteration_profile: str | None = None
    #: Which agent design dimension is varied. Manifests that lack this field
    #: are read as graph-search.
    axis: str = AXIS_GRAPH_SEARCH
    #: The graph-search surface held fixed for a topology or composition
    #: sweep. ``None`` for the graph-search axis, where ``modes`` itself is
    #: the varying surface.
    graph_search_mode: str | None = None
    #: The composition policy supplied to every episode, when
    #: composition is *not* the varying axis. Ignored on
    #: :data:`AXIS_COMPOSITION`, where ``modes`` holds the varying policy
    #: identities instead and each cell supplies its own. Defaulting to
    #: ``"deterministic"`` keeps a run's identity unchanged unless a
    #: treatment explicitly asks for the model chooser.
    composition_policy: str = "deterministic"
    #: The task profile requested through the topology-work affordance. The
    #: default preserves answer-shaped topology work; editing sweeps select
    #: ``careful`` or another accepted intent explicitly.
    topology_intent: str = "answer"
    #: Whether each episode's transcript (agent stdout/stderr and session
    #: files) is kept under ``logs/``. Off by default: a kept transcript holds
    #: the issue text and raw model output, which a run directory otherwise
    #: never stores.
    keep_transcripts: bool = False
    #: The agent loop's turn limit every episode runs under.
    max_turns: int = DEFAULT_MAX_TURNS
    #: The per-request output token limit every episode runs under.
    max_output_tokens: int = DEFAULT_MAX_OUTPUT_TOKENS
    #: The domain this run's gate-plan turns select,
    #: from :data:`DOMAINS`. ``"none"`` (the default) runs the same plan
    #: every episode did before domain plans existed, regardless of ``arm``.
    domain: str = "none"
    #: The plan/notes treatment the domain above selects, from :data:`ARMS`.
    #: ``"generic"`` (the default) is the one arm every domain, including
    #: ``"none"``, always pairs with.
    arm: str = "generic"
    #: Names (never values) of any lean switch or :data:`MAX_COST_VAR` present
    #: in the host environment when this run was configured — see
    #: :func:`evals.core.config.lean_passthrough_names`. Captured once, here,
    #: rather than re-read wherever an episode's environment is built, so a
    #: later ``recompute`` of this same manifest reaches the same recorded
    #: names even if the host environment has since changed.
    lean_env_names: tuple[str, ...] = ()

    def episode_count(self, instance_total: int) -> int:
        """Episodes one full pass of this configuration schedules."""
        return instance_total * len(self.modes) * self.repetitions

    def to_json(self) -> dict[str, object]:
        """The manifest projection of this configuration."""
        return {
            "eval_run_id": self.eval_run_id,
            "dataset_pack": self.dataset_pack,
            "model": self.model,
            "modes": list(self.modes),
            "axis": self.axis,
            "graph_search_mode": self.graph_search_mode,
            "repetitions": self.repetitions,
            "order_seed": self.order_seed,
            "analysis_seed": self.analysis_seed,
            "instance_ids": list(self.instance_ids),
            "episode_timeout_seconds": self.episode_timeout_seconds,
            "grade_timeout_seconds": self.grade_timeout_seconds,
            "max_workers": self.max_workers,
            "api_cost_ceiling_usd": self.api_cost_ceiling_usd,
            "planning_cost_per_episode_usd": PLANNING_COST_PER_EPISODE_USD,
            "polaris_code_binary": self.polaris_code_binary,
            "container_runtime": self.container_runtime,
            "allow_edit": self.allow_edit,
            "allow_command": list(self.allow_command),
            "topology_work": self.topology_work,
            "process_runner": self.process_runner,
            "allowed_env_names": list(ALLOWED_ENV_NAMES),
            "lean_env_names": list(self.lean_env_names),
            "evidence_bar": dataclasses.asdict(self.evidence_bar),
            "iteration_profile": self.iteration_profile,
            "composition_policy": self.composition_policy,
            "topology_intent": self.topology_intent,
            "keep_transcripts": self.keep_transcripts,
            "max_turns": self.max_turns,
            "max_output_tokens": self.max_output_tokens,
            "domain": self.domain,
            "arm": self.arm,
            # The label itself, not only the name it follows from: a reader
            # holding this manifest must not have to look a profile up in a
            # table to learn that the directory is not a measurement.
            "not_evidence": not_evidence_label(
                process_runner=self.process_runner,
                iteration_profile=self.iteration_profile,
            ),
        }


def normalise_modes(values: Sequence[str]) -> tuple[str, ...]:
    """Validate a ``--modes`` selection and return it in canonical order.

    Raises:
        ValueError: if a value is not one of :data:`MODES`, if the selection is
            empty, or if it repeats a mode. Silently de-duplicating would make
            a cell's repetition count disagree with the manifest.
    """
    seen: list[str] = []
    for value in values:
        if value not in MODES:
            raise ValueError(
                f"unknown graph-search mode {value!r}: expected one of {', '.join(MODES)}"
            )
        if value in seen:
            raise ValueError(f"mode {value!r} selected more than once")
        seen.append(value)
    if not seen:
        raise ValueError("at least one graph-search mode is required")
    return tuple(mode for mode in MODES if mode in seen)


def normalise_topologies(values: Sequence[str]) -> tuple[str, ...]:
    """Validate a topology selection and return it in catalog order.

    A duplicate is a configuration error, not an invitation to silently run a
    cell fewer times than its manifest says. Registration and runtime admission
    are distinct: this validates a stable catalog identity before an episode
    starts; the binary records an ineligible pinned topology as that episode's
    topology result.
    """
    seen: list[str] = []
    for value in values:
        if value not in TOPOLOGIES:
            raise ValueError(
                f"unknown topology {value!r}: expected one of {', '.join(TOPOLOGIES)}"
            )
        if value in seen:
            raise ValueError(f"topology {value!r} selected more than once")
        seen.append(value)
    if not seen:
        raise ValueError("at least one topology is required")
    return tuple(topology for topology in TOPOLOGIES if topology in seen)


def normalise_composition_policies(values: Sequence[str]) -> tuple[str, ...]:
    """Validate a ``--composition-policies`` selection, return canonical order.

    Mirrors :func:`normalise_topologies`: an unknown or repeated policy is a
    configuration error caught before an episode starts, not a silent dedup
    that would make a cell's repetition count disagree with the manifest.
    """
    seen: list[str] = []
    for value in values:
        if value not in COMPOSITION_POLICIES:
            raise ValueError(
                f"unknown composition policy {value!r}: expected one of "
                f"{', '.join(COMPOSITION_POLICIES)}"
            )
        if value in seen:
            raise ValueError(f"composition policy {value!r} selected more than once")
        seen.append(value)
    if not seen:
        raise ValueError("at least one composition policy is required")
    return tuple(policy for policy in COMPOSITION_POLICIES if policy in seen)


def normalise_domain_selection(domain: str, arm: str) -> tuple[str, str]:
    """Validate one (domain, arm) pair, mirroring ``DomainSelection::resolve``.

    Unlike :func:`normalise_modes`/:func:`normalise_topologies`/
    :func:`normalise_composition_policies`, domain and arm are not a sweep
    axis here — a run selects exactly one pair, run once per (domain, arm) —
    so this validates a single value each rather than a comma-separated list.

    Raises:
        ValueError: if `domain` is not in :data:`DOMAINS`, if `arm` is not in
            :data:`ARMS`, if `arm` is ``"generic-notes"``, ``"expert"``,
            ``"experience"`` or ``"composed"`` while `domain` is ``"none"`` (none
            has notes or a plan to read without a domain), or if `arm` is
            ``"loop"`` or ``"fine"`` while `domain` is not ``"none"`` (they read no domain
            plan or notes, so pairing it with one would carry a selection
            that changes nothing).
    """
    if domain not in DOMAINS:
        raise ValueError(f"unknown domain {domain!r}: expected one of {', '.join(DOMAINS)}")
    if arm not in ARMS:
        raise ValueError(f"unknown arm {arm!r}: expected one of {', '.join(ARMS)}")
    if domain == "none" and arm not in ("generic", "loop", "lean", "fine", "fine-ctx"):
        raise ValueError(f"arm {arm!r} needs a domain other than none")
    if arm in ("loop", "lean", "fine", "fine-ctx") and domain != "none":
        raise ValueError(f"arm {arm!r} reads no domain plan; it runs only with domain none")
    return domain, arm
