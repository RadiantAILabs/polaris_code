"""One episode: the exact invocation, a clean reset, and nothing leaked.

The invocation is the whole contract between this adapter and the shipped
agent. If it drifts — an extra flag, an added sentence, a reused session — the
experiment silently stops being a comparison of graph-search modes, so these
tests assert the argv itself rather than its effect.
"""

from __future__ import annotations

import json
import unittest

from ....core.config import (
    ALLOWED_ENV_NAMES,
    API_KEY_VAR,
    COMPOSITION_POLICY_VAR,
    CONTAINER_BINARY_PATH,
    GRAPH_SEARCH_VAR,
    MODEL_VAR,
    TOPOLOGY_VAR,
)
from ....core.episode import (
    ELIDED_PROMPT,
    REDACTED_VALUE,
    TERMINATION_AGENT_FAILED,
    TERMINATION_COMPLETED,
    TERMINATION_INFRASTRUCTURE,
    TERMINATION_TIMEOUT,
    TERMINATION_UNREADABLE_RESULT,
    agent_argv,
    agent_env,
    container_name,
    redact_command_line,
    session_id,
    statement_task_spec,
)
from ....core.process import ReplayRunner, ReplayStep
from ..environment import TESTBED, EpisodeRunner
from ..replay_script import ScriptedEpisode, episode_steps
from .support import tiny_suite


class TheOneAgentInvocation(unittest.TestCase):
    """``agent_argv`` is the supported non-interactive entry point, once."""

    def test_it_is_the_documented_invocation(self) -> None:
        argv = agent_argv(
            working_dir=TESTBED,
            problem_statement="fix the bug",
            session="run-curated-1-abc",
            allow_edit=True,
            allow_command=("cargo *",),
        )
        self.assertEqual(
            argv,
            (
                CONTAINER_BINARY_PATH,
                TESTBED,
                "-p",
                "fix the bug",
                "--output-format",
                "json",
                "--session",
                "run-curated-1-abc",
                "--allow-edit",
                "--allow-command",
                "cargo *",
            ),
        )

    def test_the_problem_statement_reaches_the_agent_byte_for_byte(self) -> None:
        # No prefix, no suffix, no benchmark hint and no test name. The prompt
        # is one argv element, so no shell can requote it either.
        statement = "Title\n\n```rust\nlet x = 1;\n```\n\nquotes ' \" $(uh oh) and — dashes\n"
        argv = agent_argv(
            working_dir=TESTBED, problem_statement=statement, session="s", allow_edit=False
        )
        self.assertEqual(argv[3], statement)
        self.assertEqual(argv.count(statement), 1)

    def test_no_grant_means_no_grant_flag(self) -> None:
        argv = agent_argv(
            working_dir=TESTBED, problem_statement="x", session="s", allow_edit=False
        )
        self.assertNotIn("--allow-edit", argv)
        self.assertNotIn("--allow-command", argv)

    def test_topology_work_submits_the_verbatim_statement_to_the_existing_affordance(self) -> None:
        statement = "fix this issue\nwith details"
        argv = agent_argv(
            working_dir=TESTBED,
            problem_statement=statement,
            session="s",
            allow_edit=True,
            topology_task=statement_task_spec(statement, "instance-1"),
        )
        self.assertIn("--topology-task", argv)
        self.assertNotIn("-p", argv)
        spec = argv[argv.index("--topology-task") + 1]
        # The grammar's quoted value round-trips the raw statement; it has to
        # escape the newline while still remaining one argv element.
        self.assertIn(json.dumps(statement), spec)
        self.assertIn("intent=answer topology=code.tree-question", spec)

    def test_the_environment_carries_the_mode_and_nothing_undeclared(self) -> None:
        env = agent_env("anthropic/claude-sonnet-4-6", "freeform", "sk-secret")
        self.assertEqual(env[GRAPH_SEARCH_VAR], "freeform")
        self.assertEqual(env[MODEL_VAR], "anthropic/claude-sonnet-4-6")
        self.assertEqual(env[COMPOSITION_POLICY_VAR], "deterministic")
        self.assertEqual(set(env) - set(ALLOWED_ENV_NAMES), set())

    def test_a_missing_key_is_omitted_rather_than_passed_empty(self) -> None:
        # An empty key would authenticate-fail mid-turn; an absent one produces
        # the binary's own exit-2 configuration error before anything is spent.
        self.assertNotIn(API_KEY_VAR, agent_env("m", "both", None))
        self.assertNotIn(API_KEY_VAR, agent_env("m", "both", ""))


class EveryEpisodeIsIsolated(unittest.TestCase):
    """No session, container or working tree is shared between cells."""

    def test_session_identity_separates_every_cell_of_the_matrix(self) -> None:
        ids = {
            session_id("run", mode, repetition, instance)
            for mode in ("curated", "freeform", "both")
            for repetition in (1, 2, 3)
            for instance in ("a__1", "b__2")
        }
        self.assertEqual(len(ids), 18)

    def test_container_names_separate_the_same_cells(self) -> None:
        names = {
            container_name("run", mode, repetition, instance)
            for mode in ("curated", "freeform", "both")
            for repetition in (1, 2)
            for instance in ("a__1", "b__2")
        }
        self.assertEqual(len(names), 12)

    def test_a_container_name_stays_legal_for_a_container_runtime(self) -> None:
        name = container_name("run/with spaces", "curated", 1, "org__repo-123")
        self.assertLessEqual(len(name), 120)
        self.assertTrue(all(c.isalnum() or c in "._-" for c in name), name)


class ContainerLifecycle(unittest.TestCase):
    """What the runner actually issues, in order, for one episode."""

    def setUp(self) -> None:
        self.suite = tiny_suite()
        self.instance = self.suite.instances[0]

    def _run(self, scripted: ScriptedEpisode, **kwargs: object):
        runner = ReplayRunner(
            episode_steps(instance=self.instance, scripted=scripted, eval_run_id="run")
        )
        episode = EpisodeRunner(
            runner,
            container_runtime="docker",
            host_binary="/host/polaris-code",
            episode_timeout_seconds=1800,
            allow_edit=True,
            api_key="sk-secret",
            clock=iter([0.0, 12.5]).__next__,
            **kwargs,  # type: ignore[arg-type]
        )
        outcome = episode.run(
            instance=self.instance,
            mode=scripted.mode,
            repetition=scripted.repetition,
            attempt=1,
            eval_run_id="run",
            model="anthropic/claude-sonnet-4-6",
        )
        return outcome, [" ".join(argv) for argv in runner.issued]

    def test_a_completed_episode_runs_the_whole_lifecycle_and_collects_a_patch(self) -> None:
        scripted = ScriptedEpisode(
            instance_id=self.instance.instance_id,
            mode="curated",
            patch="diff --git a/x b/x\n",
            input_tokens=7,
            output_tokens=3,
            estimated_cost_usd=0.5,
        )
        outcome, issued = self._run(scripted)
        self.assertEqual(outcome.termination, TERMINATION_COMPLETED)
        self.assertEqual(outcome.patch, "diff --git a/x b/x\n")
        self.assertEqual(outcome.patch_bytes, len(outcome.patch.encode()))
        self.assertTrue(outcome.tree_was_clean)
        self.assertEqual(outcome.head_commit, self.instance.base_commit)
        self.assertAlmostEqual(outcome.inference_seconds, 12.5)
        self.assertIsNone(outcome.topology_terminal)
        # The container is created, used and destroyed exactly once.
        self.assertEqual(sum(1 for line in issued if " create --name " in line), 1)
        self.assertEqual(sum(1 for line in issued if " rm -f " in line), 1)
        self.assertTrue(issued[-1].endswith(container_name("run", "curated", 1, self.instance.instance_id)))
        self.assertTrue(any(" -p " in line for line in issued if "--output-format json" in line))

    def test_the_mode_reaches_the_container_as_the_graph_search_variable(self) -> None:
        for mode in ("curated", "freeform", "both"):
            with self.subTest(mode=mode):
                scripted = ScriptedEpisode(instance_id=self.instance.instance_id, mode=mode)
                _, issued = self._run(scripted)
                turn = [line for line in issued if "--output-format json" in line]
                self.assertEqual(len(turn), 1)
                self.assertIn(f"--env {GRAPH_SEARCH_VAR}={mode}", turn[0])

    def test_a_topology_episode_records_the_runtime_terminal_answer(self) -> None:
        scripted = ScriptedEpisode(
            instance_id=self.instance.instance_id,
            mode="curated",
            topology_terminal="executed",
        )
        outcome, issued = self._run(scripted, topology_work=True)
        self.assertEqual(outcome.topology_terminal, "executed")
        turn = next(line for line in issued if "--output-format json" in line)
        self.assertIn("--topology-task", turn)

    def test_a_topology_axis_pins_the_cell_and_holds_graph_search_fixed(self) -> None:
        scripted = ScriptedEpisode(
            instance_id=self.instance.instance_id,
            mode="code.direct-edit",
            topology_terminal="no-eligible-candidate",
        )
        outcome, issued = self._run(
            scripted,
            topology_work=True,
            topology_axis=True,
            graph_search_mode="both",
        )
        self.assertEqual(outcome.topology_terminal, "no-eligible-candidate")
        turn = next(line for line in issued if "--output-format json" in line)
        self.assertIn(f"--env {GRAPH_SEARCH_VAR}=both", turn)
        self.assertIn(f"--env {TOPOLOGY_VAR}=code.direct-edit", turn)
        self.assertIn("--topology-task", turn)
        self.assertNotIn("topology=code.tree-question", turn)

    def test_a_composition_axis_selects_the_cells_policy_and_holds_graph_search_and_topology_fixed(
        self,
    ) -> None:
        scripted = ScriptedEpisode(instance_id=self.instance.instance_id, mode="model")
        _, issued = self._run(
            scripted,
            topology_work=True,
            composition_axis=True,
            graph_search_mode="both",
        )
        turn = next(line for line in issued if "--output-format json" in line)
        self.assertIn(f"--env {GRAPH_SEARCH_VAR}=both", turn)
        self.assertIn(f"--env {COMPOSITION_POLICY_VAR}=model", turn)
        self.assertIn("--topology-task", turn)
        self.assertNotIn(TOPOLOGY_VAR, turn)
        self.assertNotIn("topology=code.tree-question", turn)

    def test_a_composition_episode_records_the_assembled_recipe_and_digest(self) -> None:
        scripted = ScriptedEpisode(
            instance_id=self.instance.instance_id,
            mode="model",
            composition_policy="model",
            composition_recipe="direct-verified@v1",
            composition_digest="sha256:" + "a" * 64,
            composition_forced=False,
        )
        outcome, _ = self._run(scripted, topology_work=True, composition_axis=True)
        self.assertEqual(outcome.composition_policy, "model")
        self.assertEqual(outcome.composition_recipe, "direct-verified@v1")
        self.assertEqual(outcome.composition_digest, "sha256:" + "a" * 64)
        self.assertEqual(outcome.composition_forced, False)
        rendered = outcome.to_json("test")
        self.assertEqual(rendered["composition_policy"], "model")
        self.assertEqual(rendered["composition_recipe"], "direct-verified@v1")
        self.assertEqual(rendered["composition_digest"], "sha256:" + "a" * 64)
        self.assertEqual(rendered["composition_forced"], False)

    def test_a_conversational_episode_carries_no_composition_identity(self) -> None:
        scripted = ScriptedEpisode(instance_id=self.instance.instance_id, mode="curated")
        outcome, _ = self._run(scripted)
        self.assertIsNone(outcome.composition_policy)
        self.assertIsNone(outcome.composition_recipe)
        self.assertIsNone(outcome.composition_digest)
        self.assertIsNone(outcome.composition_forced)

    def test_episode_records_distinguish_every_selection_terminal_answer(self) -> None:
        for terminal in ("executed", "no-eligible-candidate", "not-started"):
            with self.subTest(terminal=terminal):
                outcome, _ = self._run(
                    ScriptedEpisode(
                        instance_id=self.instance.instance_id,
                        mode="curated",
                        topology_terminal=terminal,
                    ),
                    topology_work=True,
                )
                self.assertEqual(outcome.to_json("test")["topology_terminal"], terminal)

    def test_a_container_that_is_not_at_the_pinned_base_commit_is_refused(self) -> None:
        # A container whose repository already moved would measure a different
        # task than the one the suite pins, so it is an infrastructure failure
        # rather than a result.
        steps = episode_steps(
            instance=self.instance,
            scripted=ScriptedEpisode(instance_id=self.instance.instance_id, mode="both"),
            eval_run_id="run",
        )
        steps = [
            ReplayStep(match=s.match, stdout="deadbeef\n") if "rev-parse" in s.match else s
            for s in steps
        ]
        runner = ReplayRunner(steps)
        outcome = EpisodeRunner(
            runner,
            container_runtime="docker",
            host_binary="/host/polaris-code",
            episode_timeout_seconds=1800,
            allow_edit=True,
        ).run(
            instance=self.instance,
            mode="both",
            repetition=1,
            attempt=1,
            eval_run_id="run",
            model="m",
        )
        self.assertEqual(outcome.termination, TERMINATION_INFRASTRUCTURE)
        self.assertFalse(outcome.agent_was_launched)
        self.assertIn("not the pinned base commit", outcome.error or "")
        # The turn was never issued, so nothing could have been spent.
        self.assertFalse(any("--output-format json" in " ".join(a) for a in runner.issued))

    def test_a_dirty_container_repository_is_refused_before_the_turn(self) -> None:
        steps = episode_steps(
            instance=self.instance,
            scripted=ScriptedEpisode(instance_id=self.instance.instance_id, mode="both"),
            eval_run_id="run",
        )
        steps = [
            ReplayStep(match=s.match, stdout=" M src/lib.rs\n")
            if "status --porcelain" in s.match
            else s
            for s in steps
        ]
        outcome = EpisodeRunner(
            ReplayRunner(steps),
            container_runtime="docker",
            host_binary="/host/polaris-code",
            episode_timeout_seconds=1800,
            allow_edit=True,
        ).run(
            instance=self.instance, mode="both", repetition=1, attempt=1,
            eval_run_id="run", model="m",
        )
        self.assertEqual(outcome.termination, TERMINATION_INFRASTRUCTURE)
        self.assertFalse(outcome.tree_was_clean)

    def test_a_container_that_cannot_be_created_never_launches_the_agent(self) -> None:
        scripted = ScriptedEpisode(
            instance_id=self.instance.instance_id, mode="both", container_failure="no such image"
        )
        outcome, issued = self._run(scripted)
        self.assertEqual(outcome.termination, TERMINATION_INFRASTRUCTURE)
        self.assertFalse(outcome.agent_was_launched)
        self.assertIsNone(outcome.usage)
        self.assertFalse(any("--output-format json" in line for line in issued))

    def test_a_failed_turn_is_a_result_and_keeps_its_accounting(self) -> None:
        scripted = ScriptedEpisode(
            instance_id=self.instance.instance_id,
            mode="both",
            status="failed",
            exit_code=1,
            input_tokens=11,
            estimated_cost_usd=0.02,
        )
        outcome, _ = self._run(scripted)
        self.assertEqual(outcome.termination, TERMINATION_AGENT_FAILED)
        self.assertTrue(outcome.agent_was_launched)
        self.assertIsNotNone(outcome.usage)
        self.assertEqual(outcome.usage.input_tokens, 11)  # type: ignore[union-attr]
        self.assertEqual(outcome.exit_code, 1)

    def test_a_timeout_is_a_result_with_an_empty_patch(self) -> None:
        steps = episode_steps(
            instance=self.instance,
            scripted=ScriptedEpisode(instance_id=self.instance.instance_id, mode="both"),
            eval_run_id="run",
        )
        steps = [
            ReplayStep(match=s.match, timed_out=True, returncode=-62)
            if "--output-format" in s.match
            else s
            for s in steps
        ]
        outcome = EpisodeRunner(
            ReplayRunner(steps),
            container_runtime="docker",
            host_binary="/host/polaris-code",
            episode_timeout_seconds=1800,
            allow_edit=True,
        ).run(
            instance=self.instance, mode="both", repetition=1, attempt=1,
            eval_run_id="run", model="m",
        )
        self.assertEqual(outcome.termination, TERMINATION_TIMEOUT)
        self.assertEqual(outcome.patch, "")
        self.assertTrue(outcome.agent_was_launched)
        self.assertIn("1800", outcome.error or "")

    def test_unreadable_stdout_is_a_result_rather_than_a_crash(self) -> None:
        steps = episode_steps(
            instance=self.instance,
            scripted=ScriptedEpisode(instance_id=self.instance.instance_id, mode="both"),
            eval_run_id="run",
        )
        steps = [
            ReplayStep(match=s.match, stdout="not json at all")
            if "--output-format" in s.match
            else s
            for s in steps
        ]
        outcome = EpisodeRunner(
            ReplayRunner(steps),
            container_runtime="docker",
            host_binary="/host/polaris-code",
            episode_timeout_seconds=1800,
            allow_edit=True,
        ).run(
            instance=self.instance, mode="both", repetition=1, attempt=1,
            eval_run_id="run", model="m",
        )
        self.assertEqual(outcome.termination, TERMINATION_UNREADABLE_RESULT)
        self.assertEqual(outcome.patch, "")


class NothingSecretReachesARecord(unittest.TestCase):
    """The episode log keeps a command line; it keeps no key and no prompt."""

    def test_the_provider_key_is_redacted_and_the_prompt_is_elided(self) -> None:
        argv = [
            "docker", "exec", "--env", f"{API_KEY_VAR}=sk-ant-super-secret",
            "--env", f"{MODEL_VAR}=anthropic/claude-sonnet-4-6",
            "box", CONTAINER_BINARY_PATH, TESTBED, "-p", "the whole issue text", "--output-format", "json",
        ]
        line = redact_command_line(argv)
        self.assertNotIn("sk-ant-super-secret", line)
        self.assertIn(f"{API_KEY_VAR}={REDACTED_VALUE}", line)
        self.assertNotIn("the whole issue text", line)
        self.assertIn(f"-p {ELIDED_PROMPT}", line)
        # The model name is not a secret and stays readable.
        self.assertIn(f"{MODEL_VAR}=anthropic/claude-sonnet-4-6", line)

    def test_the_topology_task_spec_is_elided_from_command_logs(self) -> None:
        argv = [
            "docker", "exec", "box", CONTAINER_BINARY_PATH, TESTBED,
            "--topology-task", 'statement="the whole issue text" ref=instance intent=answer',
            "--output-format", "json",
        ]
        line = redact_command_line(argv)
        self.assertNotIn("the whole issue text", line)
        self.assertIn(f"--topology-task {ELIDED_PROMPT}", line)

    def test_a_recorded_episode_carries_neither_the_key_nor_the_issue_text(self) -> None:
        suite = tiny_suite()
        instance = suite.instances[0]
        scripted = ScriptedEpisode(instance_id=instance.instance_id, mode="both", patch="diff\n")
        runner = ReplayRunner(episode_steps(instance=instance, scripted=scripted, eval_run_id="run"))
        outcome = EpisodeRunner(
            runner,
            container_runtime="docker",
            host_binary="/host/polaris-code",
            episode_timeout_seconds=1800,
            allow_edit=True,
            api_key="sk-ant-super-secret",
        ).run(
            instance=instance, mode="both", repetition=1, attempt=1,
            eval_run_id="run", model="m",
        )
        serialized = json.dumps(outcome.to_json("schema"))
        self.assertNotIn("sk-ant-super-secret", serialized)
        self.assertNotIn(instance.problem_statement.strip(), serialized)
        # The patch itself is stored beside the record, not inside it.
        self.assertNotIn("patch", set(outcome.to_json("schema")) - {"patch_sha256", "patch_bytes"})


if __name__ == "__main__":
    unittest.main()
