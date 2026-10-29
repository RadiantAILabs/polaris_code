"""Verified starts every disposable checkout at the row's base commit."""

from __future__ import annotations

import subprocess
import tempfile
import unittest
from unittest.mock import patch
from pathlib import Path
from typing import Mapping, Sequence

from ....core.config import (
    ALLOWED_ENV_NAMES,
    COMPOSITION_POLICY_VAR,
    GATE_PLANNER_VAR,
    GRAPH_SEARCH_VAR,
    lean_passthrough_names,
)
from ....core.episode import agent_env, statement_task_spec, task_field_quote
from ....core.process import Completed
from ....core.runner import _write_patch
from ..environment import EpisodeRunner, testbed_command
from ..suite import Instance


class RecordingRunner:
    """A successful container runner that records the adapter's argv."""

    kind = "replay"

    def __init__(self, base_commit: str) -> None:
        self.base_commit = base_commit
        self.issued: list[tuple[str, ...]] = []

    def run(
        self,
        argv: Sequence[str],
        *,
        env: Mapping[str, str] | None = None,
        timeout: float | None = None,
        cwd=None,
    ) -> Completed:
        del env, timeout, cwd
        recorded = tuple(argv)
        self.issued.append(recorded)
        stdout = ""
        if recorded[1:3] == ("image", "inspect"):
            stdout = "image@sha256:abc\n"
        elif recorded[-3:] == ("git", "rev-parse", "HEAD"):
            stdout = f"{self.base_commit}\n"
        return Completed(argv=recorded, returncode=0, stdout=stdout, stderr="")


class ProjectEnvironment(unittest.TestCase):
    """Activation is host setup, with no shell interpretation of agent args."""

    def command(self, value=None):
        runner = EpisodeRunner(
            RecordingRunner("base"), container_runtime="docker",
            host_binary="/bin/true", episode_timeout_seconds=1,
            allow_edit=True, allow_command=("python *", "pytest *"), arm="loop",
            lean_env={} if value is None else {"POLARIS_CODE_TESTENV": value},
        )
        return runner._agent_command("container", "model", "both", "issue", "session", "instance")  # noqa: SLF001

    def test_activation_only_when_switched_on(self):
        command = self.command(value="1")
        self.assertIn("/bin/bash", command)
        self.assertIn('exec "$@"', command[command.index("-c") + 1])
        grants = [command[i + 1] for i, arg in enumerate(command) if arg == "--allow-command"]
        self.assertEqual(grants, ["python *", "pytest *"])
        for value in (None, "", "0"):
            self.assertNotIn("/bin/bash", self.command(value=value))

    def test_argv_stays_literal_through_activation(self):
        with tempfile.TemporaryDirectory() as tmp:
            profile = Path(tmp) / "conda.sh"
            profile.write_text('conda() { export CONDA_PREFIX="/project/env"; }\n')
            with patch(
                "evals.packs.swebench_verified.environment.TESTBED_ACTIVATION",
                f"source {profile} && conda activate testbed",
            ):
                literal = 'spaces; $(exit 99) "quotes"'
                result = subprocess.run(
                    testbed_command(["bash", "-c", 'printf "%s:%s" "$CONDA_PREFIX" "$1"', "x", literal]),
                    capture_output=True, text=True,
                )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, f"/project/env:{literal}")

    def test_failed_activation_never_starts_the_agent(self):
        with patch("evals.packs.swebench_verified.environment.TESTBED_ACTIVATION", "false"):
            result = subprocess.run(testbed_command(["echo", "started"]), capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")


class VerifiedCheckoutReset(unittest.TestCase):
    """The image head cannot silently replace the dataset's stated task base."""

    def test_reset_and_clean_happen_before_the_base_commit_gate(self) -> None:
        instance = Instance(
            instance_id="sympy__sympy-22914",
            repo="sympy/sympy",
            base_commit="c4e836cdf73fc6aa7bab6a86719a0f08861ffb1d",
            version="1.10",
            image="swebench/sweb.eval.x86_64.sympy_1776_sympy-22914:latest",
            problem_statement="problem",
        )
        runner = RecordingRunner(instance.base_commit)
        result = EpisodeRunner(
            runner,
            container_runtime="docker",
            host_binary="/bin/true",
            episode_timeout_seconds=1,
            allow_edit=True,
        ).run(
            instance=instance,
            mode="curated",
            repetition=1,
            attempt=1,
            eval_run_id="verified-test",
            model="model",
        )
        self.assertEqual(result.base_commit, instance.base_commit)
        reset = ("docker", "exec", "--workdir", "/testbed")
        reset_index = next(
            index
            for index, argv in enumerate(runner.issued)
            if argv[:4] == reset and argv[-4:-1] == ("git", "reset", "--hard")
        )
        # The precise base SHA follows the three-token git reset command.
        self.assertEqual(runner.issued[reset_index][-1], instance.base_commit)
        clean_index = next(
            index
            for index, argv in enumerate(runner.issued)
            if argv[:4] == reset and argv[-3:] == ("git", "clean", "-fdx")
        )
        head_index = next(
            index
            for index, argv in enumerate(runner.issued)
            if argv[-3:] == ("git", "rev-parse", "HEAD")
        )
        self.assertLess(reset_index, clean_index)
        self.assertLess(clean_index, head_index)


class VerifiedTopologyCompositionInvocation(unittest.TestCase):
    """Verified composition cells use the shared topology-work contract."""

    def test_a_gates_model_cell_holds_graph_search_fixed_and_requests_careful_work(self) -> None:
        instance = Instance(
            instance_id="sympy__sympy-22914",
            repo="sympy/sympy",
            base_commit="c4e836cdf73fc6aa7bab6a86719a0f08861ffb1d",
            version="1.10",
            image="swebench/sweb.eval.x86_64.sympy_1776_sympy-22914:latest",
            problem_statement="problem",
        )
        runner = RecordingRunner(instance.base_commit)
        EpisodeRunner(
            runner,
            container_runtime="docker",
            host_binary="/bin/true",
            episode_timeout_seconds=1,
            allow_edit=True,
            topology_work=True,
            composition_axis=True,
            graph_search_mode="both",
            topology_intent="careful",
        ).run(
            instance=instance,
            mode="gates-model",
            repetition=1,
            attempt=1,
            eval_run_id="verified-test",
            model="model",
        )
        turn = next(argv for argv in runner.issued if "--output-format" in argv)
        self.assertIn(f"{GRAPH_SEARCH_VAR}=both", turn)
        self.assertIn(f"{COMPOSITION_POLICY_VAR}=gates", turn)
        self.assertIn(f"{GATE_PLANNER_VAR}=model", turn)
        self.assertNotIn("POLARIS_CODE_TOPOLOGY_PIN=", " ".join(turn))
        task = turn[turn.index("--topology-task") + 1]
        self.assertIn("intent=careful", task)
        self.assertNotIn("topology=", task)

    def test_turn_and_output_limits_reach_the_agent_only_when_set(self) -> None:
        unset = agent_env("model", "both", None)
        self.assertNotIn("POLARIS_CODE_MAX_TURNS", unset)
        self.assertNotIn("POLARIS_CODE_MAX_OUTPUT_TOKENS", unset)
        limited = agent_env("model", "both", None, max_turns=40, max_output_tokens=16384)
        self.assertEqual(limited["POLARIS_CODE_MAX_TURNS"], "40")
        self.assertEqual(limited["POLARIS_CODE_MAX_OUTPUT_TOKENS"], "16384")

    def test_extra_env_merges_in_and_is_absent_when_not_given(self) -> None:
        plain = agent_env("model", "both", None)
        self.assertNotIn("POLARIS_CODE_LEAN_TWM", plain)
        merged = agent_env(
            "model", "both", None,
            extra_env={"POLARIS_CODE_LEAN_TWM": "1", "POLARIS_CODE_MAX_COST_USD": "0.5"},
        )
        self.assertEqual(merged["POLARIS_CODE_LEAN_TWM"], "1")
        self.assertEqual(merged["POLARIS_CODE_MAX_COST_USD"], "0.5")
        # The fixed names are unaffected by an unrelated extra_env merge.
        self.assertEqual(merged["POLARIS_CODE_MODEL"], "model")

    def test_lean_passthrough_names_matches_the_prefix_and_the_one_named_var(self) -> None:
        names = lean_passthrough_names({
            "POLARIS_CODE_LEAN_TWM": "1",
            "POLARIS_CODE_LEAN_TWM_REBUILD_TURNS": "4",
            "POLARIS_CODE_MAX_COST_USD": "0.5",
            "POLARIS_CODE_MODEL": "unrelated",
            "PATH": "unrelated",
        })
        self.assertEqual(
            names,
            (
                "POLARIS_CODE_LEAN_TWM",
                "POLARIS_CODE_LEAN_TWM_REBUILD_TURNS",
                "POLARIS_CODE_MAX_COST_USD",
            ),
        )
        self.assertEqual(lean_passthrough_names({}), ())

    def test_gate_policy_aliases_expand_to_the_binarys_two_environment_settings(self) -> None:
        scripted = agent_env("model", "both", None, composition_policy="gates")
        model = agent_env("model", "both", None, composition_policy="gates-model")
        self.assertEqual(scripted[COMPOSITION_POLICY_VAR], "gates")
        self.assertEqual(scripted[GATE_PLANNER_VAR], "scripted")
        self.assertEqual(model[COMPOSITION_POLICY_VAR], "gates")
        self.assertEqual(model[GATE_PLANNER_VAR], "model")
        self.assertTrue(set(scripted).issubset(ALLOWED_ENV_NAMES))
        self.assertTrue(set(model).issubset(ALLOWED_ENV_NAMES))

    def test_statement_task_spec_carries_the_selected_intent(self) -> None:
        spec = statement_task_spec("problem", "sympy__sympy-22914", intent="thorough")
        self.assertIn("intent=thorough", spec)


class DomainAndArmReachTheAgentAndTheRecord(unittest.TestCase):
    """A selected domain/arm reaches the container's own environment and the record."""

    def _instance(self) -> Instance:
        return Instance(
            instance_id="sympy__sympy-22914",
            repo="sympy/sympy",
            base_commit="c4e836cdf73fc6aa7bab6a86719a0f08861ffb1d",
            version="1.10",
            image="swebench/sweb.eval.x86_64.sympy_1776_sympy-22914:latest",
            problem_statement="problem",
        )

    def test_a_selected_domain_and_arm_reach_the_containers_environment(self) -> None:
        instance = self._instance()
        runner = RecordingRunner(instance.base_commit)
        EpisodeRunner(
            runner,
            container_runtime="docker",
            host_binary="/bin/true",
            episode_timeout_seconds=1,
            allow_edit=True,
            domain="django",
            arm="expert",
        ).run(
            instance=instance, mode="both", repetition=1, attempt=1,
            eval_run_id="verified-test", model="model",
        )
        agent_call = next(argv for argv in runner.issued if "--output-format" in argv)
        self.assertIn("--env", agent_call)
        self.assertIn("POLARIS_CODE_DOMAIN=django", agent_call)
        self.assertIn("POLARIS_CODE_ARM=expert", agent_call)

    def test_the_default_domain_and_arm_are_set_explicitly_not_omitted(self) -> None:
        # Unlike a truly optional knob (a topology pin, a turn limit), domain
        # and arm are always sent: an explicit "none"/"generic" is not the
        # same as leaving the binary's own default unstated (see
        # `evals.core.episode.agent_env`).
        instance = self._instance()
        runner = RecordingRunner(instance.base_commit)
        EpisodeRunner(
            runner, container_runtime="docker", host_binary="/bin/true",
            episode_timeout_seconds=1, allow_edit=True,
        ).run(
            instance=instance, mode="both", repetition=1, attempt=1,
            eval_run_id="verified-test", model="model",
        )
        agent_call = next(argv for argv in runner.issued if "--output-format" in argv)
        self.assertIn("POLARIS_CODE_DOMAIN=none", agent_call)
        self.assertIn("POLARIS_CODE_ARM=generic", agent_call)

    def test_lean_env_reaches_the_container_by_name_and_value(self) -> None:
        instance = self._instance()
        runner = RecordingRunner(instance.base_commit)
        EpisodeRunner(
            runner,
            container_runtime="docker",
            host_binary="/bin/true",
            episode_timeout_seconds=1,
            allow_edit=True,
            lean_env={"POLARIS_CODE_LEAN_TWM": "1", "POLARIS_CODE_MAX_COST_USD": "0.5"},
        ).run(
            instance=instance, mode="both", repetition=1, attempt=1,
            eval_run_id="verified-test", model="model",
        )
        agent_call = next(argv for argv in runner.issued if "--output-format" in argv)
        self.assertIn("POLARIS_CODE_LEAN_TWM=1", agent_call)
        self.assertIn("POLARIS_CODE_MAX_COST_USD=0.5", agent_call)

    def test_aws_credentials_reach_the_container_by_name_only(self) -> None:
        instance = self._instance()
        runner = RecordingRunner(instance.base_commit)
        EpisodeRunner(
            runner,
            container_runtime="docker",
            host_binary="/bin/true",
            episode_timeout_seconds=1,
            allow_edit=True,
            lean_env={
                "POLARIS_CODE_PROVIDER": "bedrock",
                "AWS_ACCESS_KEY_ID": "AKIAEXAMPLE",
                "AWS_SECRET_ACCESS_KEY": "secret-value",
                "AWS_SESSION_TOKEN": "token-value",
            },
        ).run(
            instance=instance, mode="both", repetition=1, attempt=1,
            eval_run_id="verified-test", model="model",
        )
        agent_call = next(argv for argv in runner.issued if "--output-format" in argv)
        self.assertIn("POLARIS_CODE_PROVIDER=bedrock", agent_call)
        for name in ("AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN"):
            self.assertIn(name, agent_call)
        for value in ("AKIAEXAMPLE", "secret-value", "token-value"):
            self.assertFalse(any(value in token for token in agent_call))

    def test_no_lean_env_adds_nothing_to_the_container_command(self) -> None:
        instance = self._instance()
        runner = RecordingRunner(instance.base_commit)
        EpisodeRunner(
            runner, container_runtime="docker", host_binary="/bin/true",
            episode_timeout_seconds=1, allow_edit=True,
        ).run(
            instance=instance, mode="both", repetition=1, attempt=1,
            eval_run_id="verified-test", model="model",
        )
        agent_call = next(argv for argv in runner.issued if "--output-format" in argv)
        self.assertFalse(any("POLARIS_CODE_LEAN_" in token for token in agent_call))

    def test_the_episode_record_carries_domain_arm_and_notes_digest(self) -> None:
        instance = self._instance()
        runner = RecordingRunner(instance.base_commit)
        outcome = EpisodeRunner(
            runner,
            container_runtime="docker",
            host_binary="/bin/true",
            episode_timeout_seconds=1,
            allow_edit=True,
            domain="sympy",
            arm="generic-notes",
            notes_digest="sha256:deadbeef",
        ).run(
            instance=instance, mode="both", repetition=1, attempt=1,
            eval_run_id="verified-test", model="model",
        )
        self.assertEqual(outcome.domain, "sympy")
        self.assertEqual(outcome.arm, "generic-notes")
        self.assertEqual(outcome.notes_digest, "sha256:deadbeef")
        record = outcome.to_json("polaris-code-harness-eval-episode/v1")
        self.assertEqual(record["domain"], "sympy")
        self.assertEqual(record["arm"], "generic-notes")
        self.assertEqual(record["notes_digest"], "sha256:deadbeef")


class TranscriptRunner(RecordingRunner):
    """Answers the agent turn with output that leaks the key, and serves a
    session store when the adapter copies ``/testbed/.polaris`` out."""

    def __init__(self, base_commit: str, key: str) -> None:
        super().__init__(base_commit)
        self.key = key

    def run(self, argv, *, env=None, timeout=None, cwd=None) -> Completed:
        recorded = tuple(argv)
        if recorded[1] == "cp" and recorded[2].endswith(":/testbed/.polaris"):
            self.issued.append(recorded)
            target = Path(recorded[3])
            (target / "sessions").mkdir(parents=True)
            (target / "sessions" / "s1.json").write_text(
                f'{{"messages": ["fix it"], "auth": "{self.key}"}}', encoding="utf-8"
            )
            return Completed(argv=recorded, returncode=0, stdout="", stderr="")
        if recorded[1] == "exec" and "--env" in recorded:
            self.issued.append(recorded)
            self.turn_env = env
            return Completed(
                argv=recorded,
                returncode=1,
                stdout=f'{{"error": "boom {self.key}"}}',
                stderr="trace",
            )
        return super().run(argv, env=env, timeout=timeout, cwd=cwd)


class VerifiedTranscriptKeeping(unittest.TestCase):
    """A failed attempt's transcript survives the container, without the key."""

    def _run(self, *, keep: bool):
        instance = Instance(
            instance_id="sympy__sympy-22914",
            repo="sympy/sympy",
            base_commit="c4e836cdf73fc6aa7bab6a86719a0f08861ffb1d",
            version="1.10",
            image="swebench/sweb.eval.x86_64.sympy_1776_sympy-22914:latest",
            problem_statement="problem",
        )
        runner = TranscriptRunner(instance.base_commit, "sk-test-secret")
        result = EpisodeRunner(
            runner,
            container_runtime="docker",
            host_binary="/bin/true",
            episode_timeout_seconds=1,
            allow_edit=True,
            api_key="sk-test-secret",
            keep_transcripts=keep,
        ).run(
            instance=instance,
            mode="curated",
            repetition=1,
            attempt=1,
            eval_run_id="verified-test",
            model="model",
        )
        return runner, result

    def test_the_key_reaches_the_turn_by_name_never_on_its_command_line(self) -> None:
        runner, _ = self._run(keep=False)
        turn = next(a for a in runner.issued if a[1] == "exec" and "--env" in a)
        self.assertTrue(all("sk-test-secret" not in arg for arg in turn))
        self.assertIn(("--env", "ANTHROPIC_API_KEY"), list(zip(turn, turn[1:])))
        self.assertEqual(runner.turn_env["ANTHROPIC_API_KEY"], "sk-test-secret")

    def test_a_failed_turn_keeps_its_output_and_session_files_with_the_key_redacted(self) -> None:
        runner, result = self._run(keep=True)
        kept = dict(result.transcript)
        self.assertEqual(
            sorted(kept), ["agent-stderr.log", "agent-stdout.json", "state/sessions/s1.json"]
        )
        self.assertIn("boom [REDACTED:ANTHROPIC_API_KEY]", kept["agent-stdout.json"])
        self.assertIn("fix it", kept["state/sessions/s1.json"])
        self.assertTrue(all("sk-test-secret" not in text for text in kept.values()))
        self.assertNotIn("transcript", result.to_json("schema"))
        copy_index = next(i for i, a in enumerate(runner.issued) if a[1] == "cp" and a[2].endswith(".polaris"))
        remove_index = next(i for i, a in enumerate(runner.issued) if a[1:3] == ("rm", "-f"))
        self.assertLess(copy_index, remove_index)

    def test_the_runner_writes_the_transcript_beside_the_attempt_patch(self) -> None:
        _, result = self._run(keep=True)
        with tempfile.TemporaryDirectory() as run_dir:
            _write_patch(Path(run_dir), result)
            attempt = Path(run_dir) / "logs" / "curated" / "1" / "sympy__sympy-22914"
            self.assertTrue((attempt / "attempt-1.patch").is_file())
            self.assertIn(
                "fix it",
                (attempt / "attempt-1" / "state" / "sessions" / "s1.json").read_text(encoding="utf-8"),
            )
            self.assertTrue((attempt / "attempt-1" / "agent-stdout.json").is_file())

    def test_nothing_is_kept_unless_asked(self) -> None:
        runner, result = self._run(keep=False)
        self.assertEqual(result.transcript, ())
        self.assertFalse(any(a[1] == "cp" and a[2].endswith(".polaris") for a in runner.issued))


def _binary_field_decode(quoted: str) -> str:
    """The binary's double-quoted field loop (``terminal/topology.rs``), as-is."""
    assert quoted[0] == '"'
    value, chars = [], iter(quoted[1:])
    for c in chars:
        if c == '"':
            return "".join(value)
        if c == "\\":
            nxt = next(chars)
            mapped = {"n": "\n", "t": "\t", "r": "\r", "\\": "\\", '"': '"'}.get(nxt)
            if mapped is None:
                raise ValueError(f"unknown escape \\{nxt}")
            value.append(mapped)
        else:
            value.append(c)
    raise ValueError("unclosed quote")


class TaskFieldQuoting(unittest.TestCase):
    """Issue text reaches the binary unchanged, whatever characters it holds."""

    def test_non_ascii_quotes_backslashes_and_control_characters_round_trip(self) -> None:
        text = 'Café "naïve" – 日本 🙂\npath C:\\new\\u1234 \x0c\x08 tab\tend\r'
        quoted = task_field_quote(text)
        self.assertEqual(_binary_field_decode(quoted), text)
        self.assertIn("Café", quoted)

    def test_the_rendered_spec_carries_the_statement_the_binary_can_read(self) -> None:
        spec = statement_task_spec("é", "ref-1", topology=None, intent="careful")
        self.assertEqual(spec, 'statement="é" ref=ref-1 intent=careful')


if __name__ == "__main__":
    unittest.main()
