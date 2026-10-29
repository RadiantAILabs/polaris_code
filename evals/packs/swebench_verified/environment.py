"""One SWE-bench Verified episode: a fresh container, one agent turn, one patch.

This is the dataset's half of an episode. The container image, the ``/testbed``
working directory and the ``git add -A`` patch convention are all SWE-bench
facts, which is why they live in the pack; the identity of an episode, the one
agent invocation and the record it produces come from
:mod:`evals.core.episode`.

The published Verified images retain commits after an instance's declared
``base_commit``. The official dataset row, not that incidental image HEAD, is
the task the agent must solve, so the disposable checkout is hard-reset to the
row's commit before it is checked for cleanliness and before the agent runs.

The container lifecycle is deliberately blunt. Every episode creates its own
container from the instance's official image, copies the binary in, runs one
turn, reads the patch back and destroys the container. Nothing is reused
between episodes or between modes, so there is no cache, session, index or
working-tree state that could carry a result from one cell into another.
"""

from __future__ import annotations

import os
import time
from typing import Mapping, Sequence

from ...core.config import API_KEY_VAR, SECRET_ENV_NAMES, CONTAINER_BINARY_PATH
from ...core.episode import (
    collect_transcript,
    TERMINATION_AGENT_FAILED,
    TERMINATION_COMPLETED,
    TERMINATION_INFRASTRUCTURE,
    TERMINATION_TIMEOUT,
    TERMINATION_UNREADABLE_RESULT,
    EpisodeOutcome,
    agent_argv,
    axis_agent_env,
    container_name,
    outcome,
    redact_command_line,
    session_id,
    topology_task_spec,
)
from ...core.process import Completed, ProcessRunner
from ...core.usage import RunResult, RunUsage, UsageError
from .suite import Instance

#: Where every official SWE-bench image checks the repository out. A dataset
#: convention, not ours, which is the whole reason it is not in the core.
TESTBED = "/testbed"

#: The official Python images install the project and its pinned dependencies
#: in this environment. Activation is host setup, outside the agent's command
#: policy.
TESTBED_ACTIVATION = "source /opt/miniconda3/etc/profile.d/conda.sh && conda activate testbed"


def testbed_command(argv: Sequence[str]) -> list[str]:
    """Activate the image's project environment, then replace the shell.

    Arguments stay separate argv entries, including issue text and grants. A
    missing or broken environment fails before the agent starts.
    """
    return ["/bin/bash", "-c", f'{TESTBED_ACTIVATION} && exec "$@"', "polaris-testbed", *argv]


class EpisodeRunner:
    """Runs one SWE-bench episode in a disposable container.

    All container interaction goes through the injected
    :class:`~evals.core.process.ProcessRunner`, which is what lets the tests
    assert the exact argv of a real run without a container runtime installed.
    """

    def __init__(
        self,
        runner: ProcessRunner,
        *,
        container_runtime: str,
        host_binary: str,
        episode_timeout_seconds: int,
        allow_edit: bool,
        allow_command: Sequence[str] = (),
        topology_work: bool = False,
        topology_axis: bool = False,
        composition_axis: bool = False,
        graph_search_mode: str | None = None,
        api_key: str | None = None,
        composition_policy: str = "deterministic",
        topology_intent: str = "answer",
        max_turns: int | None = None,
        max_output_tokens: int | None = None,
        keep_transcripts: bool = False,
        domain: str = "none",
        arm: str = "generic",
        notes_digest: str | None = None,
        lean_env: Mapping[str, str] | None = None,
        clock=time.monotonic,
    ) -> None:
        self._runner = runner
        self._runtime = container_runtime
        self._host_binary = host_binary
        self._timeout = episode_timeout_seconds
        self._allow_edit = allow_edit
        self._allow_command = tuple(allow_command)
        self._topology_work = topology_work
        self._topology_axis = topology_axis
        self._composition_axis = composition_axis
        self._graph_search_mode = graph_search_mode
        self._api_key = api_key
        self._composition_policy = composition_policy
        self._topology_intent = topology_intent
        self._max_turns = max_turns
        self._max_output_tokens = max_output_tokens
        self._keep_transcripts = keep_transcripts
        self._domain = domain
        self._arm = arm
        self._notes_digest = notes_digest
        self._lean_env = dict(lean_env) if lean_env else {}
        self._clock = clock

    def run(
        self,
        *,
        instance: Instance,
        mode: str,
        repetition: int,
        attempt: int,
        eval_run_id: str,
        model: str,
    ) -> EpisodeOutcome:
        """Run one attempt and always return an outcome, never raise.

        A container that cannot be created, a binary that cannot be copied in
        and a repository that is not at its pinned base commit are
        infrastructure failures. A turn that fails, times out or produces
        unreadable output is a result.
        """
        name = container_name(eval_run_id, mode, repetition, instance.instance_id)
        session = session_id(eval_run_id, mode, repetition, instance.instance_id)
        commands: list[str] = []
        started = self._clock()

        def fail(reason: str, exit_code: int) -> EpisodeOutcome:
            return self._outcome(
                instance=instance,
                mode=mode,
                repetition=repetition,
                attempt=attempt,
                session=session,
                termination=TERMINATION_INFRASTRUCTURE,
                exit_code=exit_code,
                result=None,
                patch="",
                image_digest=digest,
                head_commit=head,
                tree_was_clean=clean,
                error=reason,
                seconds=self._clock() - started,
                commands=commands,
            )

        digest: str | None = None
        head: str | None = None
        clean = False

        inspect_argv = [
            self._runtime, "image", "inspect",
            "--format", "{{index .RepoDigests 0}}", instance.image,
        ]
        inspect = self._exec(inspect_argv, commands)
        if not inspect.ok:
            # Absent locally is ordinary on a first run: pull, then re-read the
            # digest. A pull that fails is not itself fatal — `create` below is
            # the authoritative check that the image is usable.
            self._exec([self._runtime, "pull", instance.image], commands)
            inspect = self._exec(inspect_argv, commands)
        digest = inspect.stdout.strip() if inspect.ok else None

        created = self._exec(
            [
                self._runtime, "create",
                "--name", name,
                "--workdir", TESTBED,
                "--entrypoint", "sleep",
                instance.image,
                "infinity",
            ],
            commands,
        )
        if not created.ok:
            return fail(f"could not create container: {created.stderr.strip()[:400]}", created.returncode)

        try:
            for argv in (
                [self._runtime, "start", name],
                [self._runtime, "exec", name, "mkdir", "-p", _parent(CONTAINER_BINARY_PATH)],
                [self._runtime, "cp", self._host_binary, f"{name}:{CONTAINER_BINARY_PATH}"],
                [self._runtime, "exec", name, "chmod", "+x", CONTAINER_BINARY_PATH],
                [
                    self._runtime,
                    "exec",
                    "--workdir",
                    TESTBED,
                    name,
                    "git",
                    "reset",
                    "--hard",
                    instance.base_commit,
                ],
                [self._runtime, "exec", "--workdir", TESTBED, name, "git", "clean", "-fdx"],
            ):
                step = self._exec(argv, commands)
                if not step.ok:
                    return fail(f"container setup failed: {step.describe()}", step.returncode)

            head_probe = self._exec([self._runtime, "exec", "--workdir", TESTBED, name, "git", "rev-parse", "HEAD"], commands)
            head = head_probe.stdout.strip() if head_probe.ok else None
            if head != instance.base_commit:
                return fail(
                    f"container HEAD {head!r} is not the pinned base commit {instance.base_commit!r}",
                    head_probe.returncode,
                )

            status_probe = self._exec([self._runtime, "exec", "--workdir", TESTBED, name, "git", "status", "--porcelain"], commands)
            if not status_probe.ok:
                return fail(f"could not read the repository state: {status_probe.describe()}", status_probe.returncode)
            clean = status_probe.stdout.strip() == ""
            if not clean:
                return fail("container repository was not clean before the turn", status_probe.returncode)

            turn = self._exec(
                self._agent_command(
                    name, model, mode, instance.problem_statement, session, instance.instance_id
                ),
                commands,
                timeout=self._timeout,
                env=self._key_env(),
            )
            transcript = (
                collect_transcript(
                    lambda argv: self._exec(argv, commands),
                    runtime=self._runtime,
                    container=name,
                    workdir=TESTBED,
                    turn=turn,
                    api_key=self._api_key,
                )
                if self._keep_transcripts
                else ()
            )
            if turn.timed_out:
                return self._outcome(
                    instance=instance, mode=mode, repetition=repetition, attempt=attempt,
                    session=session, termination=TERMINATION_TIMEOUT, exit_code=turn.returncode,
                    result=None, patch="", image_digest=digest, head_commit=head,
                    tree_was_clean=clean,
                    error=f"episode exceeded {self._timeout}s", seconds=self._clock() - started,
                    commands=commands, transcript=transcript,
                )

            try:
                parsed = RunResult.parse(turn.stdout)
            except UsageError as err:
                return self._outcome(
                    instance=instance, mode=mode, repetition=repetition, attempt=attempt,
                    session=session, termination=TERMINATION_UNREADABLE_RESULT,
                    exit_code=turn.returncode, result=None, patch="", image_digest=digest,
                    head_commit=head, tree_was_clean=clean, error=str(err),
                    seconds=self._clock() - started, commands=commands,
                    transcript=transcript,
                )

            patch = self._collect_patch(name, instance.base_commit, commands)
            termination = (
                TERMINATION_COMPLETED if parsed.status == "completed" else TERMINATION_AGENT_FAILED
            )
            return self._outcome(
                instance=instance, mode=mode, repetition=repetition, attempt=attempt,
                session=session, termination=termination, exit_code=turn.returncode,
                result=parsed, patch=patch, image_digest=digest, head_commit=head,
                tree_was_clean=clean, error=parsed.error, seconds=self._clock() - started,
                commands=commands, transcript=transcript,
            )
        finally:
            self._exec([self._runtime, "rm", "-f", name], commands)

    def _agent_command(
        self,
        name: str,
        model: str,
        mode: str,
        problem_statement: str,
        session: str,
        instance_id: str,
    ) -> list[str]:
        """The container command that carries the one agent invocation."""
        argv = [self._runtime, "exec", "--workdir", TESTBED]
        for key, value in axis_agent_env(
            model,
            mode,
            self._api_key,
            topology_axis=self._topology_axis,
            composition_axis=self._composition_axis,
            graph_search_mode=self._graph_search_mode,
            composition_policy=self._composition_policy,
            max_turns=self._max_turns,
            max_output_tokens=self._max_output_tokens,
            domain=self._domain,
            arm=self._arm,
            extra_env=self._lean_env,
        ).items():
            # Values never reach a record; only the names do, and the manifest
            # lists them. `--env NAME=VALUE` is the runtime's own form. The key
            # goes by name only (`--env NAME` copies it from the runtime
            # client's environment) so it never appears in a process listing.
            argv.extend(["--env", key if key in (API_KEY_VAR, *SECRET_ENV_NAMES) else f"{key}={value}"])
        argv.append(name)
        command = agent_argv(
            working_dir=TESTBED,
            problem_statement=problem_statement,
            session=session,
            allow_edit=self._allow_edit,
            allow_command=self._allow_command,
            topology_task=topology_task_spec(
                problem_statement,
                instance_id,
                topology_work=self._topology_work,
                topology_axis=self._topology_axis,
                composition_axis=self._composition_axis,
                intent=self._topology_intent,
            ),
        )
        # Environment only: the plan, the tools and their grants are unchanged.
        # The binary keeps the activated runtime when its shell clears the rest.
        if self._lean_env.get("POLARIS_CODE_TESTENV", "").strip() == "1":
            command = testbed_command(command)
        argv.extend(command)
        return argv

    def _collect_patch(self, name: str, base_commit: str, commands: list[str]) -> str:
        """Read the agent's work back out as a prediction patch.

        ``git add -A`` then ``git diff --cached <base>`` is the shape the
        official harness expects: it captures new files as well as edits while
        still respecting the repository's own ``.gitignore``, so build output
        never reaches a prediction.
        """
        staged = self._exec([self._runtime, "exec", "--workdir", TESTBED, name, "git", "add", "-A"], commands)
        if not staged.ok:
            return ""
        diff = self._exec(
            [self._runtime, "exec", "--workdir", TESTBED, name, "git", "diff", "--cached", base_commit],
            commands,
        )
        return diff.stdout if diff.ok else ""

    def _key_env(self) -> Mapping[str, str] | None:
        """The runtime client's environment carrying the key `--env NAME` copies."""
        return {**os.environ, API_KEY_VAR: self._api_key} if self._api_key else None

    def _exec(
        self,
        argv: Sequence[str],
        commands: list[str],
        *,
        timeout: float | None = None,
        env: Mapping[str, str] | None = None,
    ) -> Completed:
        """Run one command and append a secret-free line to the episode log."""
        finished = self._runner.run(argv, timeout=timeout, env=env)
        state = "timed out" if finished.timed_out else f"exit {finished.returncode}"
        commands.append(f"{redact_command_line(argv)} -> {state}")
        return finished

    def _outcome(
        self,
        *,
        instance: Instance,
        mode: str,
        repetition: int,
        attempt: int,
        session: str,
        termination: str,
        exit_code: int,
        result: RunResult | None,
        patch: str,
        image_digest: str | None,
        head_commit: str | None,
        tree_was_clean: bool,
        error: str | None,
        seconds: float,
        commands: Sequence[str],
        transcript: Sequence[tuple[str, str]] = (),
    ) -> EpisodeOutcome:
        """Assemble the record for one attempt, filling in the image facts."""
        usage: RunUsage | None = None if result is None else result.usage
        return outcome(
            instance_id=instance.instance_id,
            mode=mode,
            repetition=repetition,
            attempt=attempt,
            session=session,
            termination=termination,
            exit_code=exit_code,
            turn_number=None if result is None else result.turn_number,
            run_id=None if result is None else result.run_id,
            status=None if result is None else result.status,
            usage=usage,
            max_output_tokens=None if result is None else result.max_output_tokens,
            patch=patch,
            image=instance.image,
            image_digest=image_digest,
            base_commit=instance.base_commit,
            head_commit=head_commit,
            tree_was_clean=tree_was_clean,
            error=error,
            seconds=seconds,
            commands=commands,
            transcript=transcript,
            topology_terminal=None if result is None else result.topology_terminal,
            composition_policy=None if result is None else result.composition_policy,
            composition_recipe=None if result is None else result.composition_recipe,
            composition_digest=None if result is None else result.composition_digest,
            composition_forced=None if result is None else result.composition_forced,
            gate_plan_digest=None if result is None else result.gate_plan_digest,
            gate_plan_shape_digest=None if result is None else result.gate_plan_shape_digest,
            gate_plan_shape=None if result is None else result.gate_plan_shape,
            gate_plan_planner=None if result is None else result.gate_plan_planner,
            gate_plan_revision_attempt=None if result is None else result.gate_plan_revision_attempt,
            gate_plan_prior_refused_attempts=(
                None if result is None else result.gate_plan_prior_refused_attempts
            ),
            exhausted_gate_plan_refused_attempts=(
                None if result is None else result.exhausted_gate_plan_refused_attempts
            ),
            domain=self._domain,
            arm=self._arm,
            notes_digest=self._notes_digest,
        )


def _parent(path: str) -> str:
    """The parent directory of an absolute container path."""
    return path.rsplit("/", 1)[0] or "/"
