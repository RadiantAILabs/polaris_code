"""The pinned suite is the denominator, the prompt and the lock.

``suite/instances.json`` is the one file in this adapter that decides what is
measured. If it can drift silently, every score derived from it drifts with
it — so these tests pin its identity as hard as the repository's other catalog
drift guards pin theirs, and prove that each way it could shrink is refused.
"""

from __future__ import annotations

import dataclasses
import json
import tempfile
import unittest
from pathlib import Path

from ..suite import SUITE_DENOMINATOR, SUITE_SCHEMA, Suite, SuiteError, instances_digest
from .support import tiny_suite

#: SHA-256 over the canonical serialization of the 43 pinned instance rows.
#: Pinned here so that editing, reordering, adding or re-wording any instance —
#: including its ``problem_statement`` — fails this test instead of silently
#: re-cutting the suite under an unchanged dataset revision.
PINNED_DIGEST = "a7fd4719c268a5ec9c0277ad077ba1fbef7e9931f9d68d8f5280279417d8212e"

#: The seven Rust repositories in the suite, with the instance count each
#: contributes. Pinned as a table rather than a total so that swapping one
#: repository's tasks for another's cannot keep the total at 43 unnoticed.
PINNED_REPOSITORIES = {
    "astral-sh/ruff": 7,
    "burntsushi/ripgrep": 2,
    "nushell/nushell": 5,
    "sharkdp/bat": 8,
    "tokio-rs/axum": 7,
    "tokio-rs/tokio": 9,
    "uutils/coreutils": 5,
}


def _write(tmp: Path, payload: dict) -> Path:
    """Write a candidate suite file and return its path."""
    path = tmp / "instances.json"
    path.write_text(json.dumps(payload), encoding="utf-8")
    return path


def _payload(suite: Suite) -> dict:
    """The on-disk form of ``suite``, as a mutable dictionary."""
    return {
        "schema": SUITE_SCHEMA,
        "dataset": suite.dataset,
        "dataset_revision": suite.dataset_revision,
        "dataset_split": suite.dataset_split,
        "dataset_file": suite.dataset_file,
        "dataset_file_sha256": suite.dataset_file_sha256,
        "dataset_file_bytes": suite.dataset_file_bytes,
        "task_repo": suite.task_repo,
        "task_repo_revision": suite.task_repo_revision,
        "grader_repo": suite.grader_repo,
        "grader_revision": suite.grader_revision,
        "language": suite.language,
        "repositories": list(suite.repositories),
        "denominator": SUITE_DENOMINATOR,
        "instances": [dataclasses.asdict(i) for i in suite.instances],
    }


class ThePinnedListIsTheRustSliceOfTheDataset(unittest.TestCase):
    """The checked-in file is the 43-instance Rust slice, not something else."""

    def setUp(self) -> None:
        self.suite = Suite.load()

    def test_it_holds_exactly_the_43_instances_of_the_rust_slice(self) -> None:
        self.assertEqual(len(self.suite.instances), SUITE_DENOMINATOR)
        self.assertEqual(self.suite.denominator, SUITE_DENOMINATOR)
        self.assertEqual(len(set(self.suite.instance_ids)), SUITE_DENOMINATOR)

    def test_every_instance_comes_from_one_of_the_seven_named_repositories(self) -> None:
        counts: dict[str, int] = {}
        for instance in self.suite.instances:
            counts[instance.repo] = counts.get(instance.repo, 0) + 1
        self.assertEqual(counts, PINNED_REPOSITORIES)
        self.assertEqual(sum(PINNED_REPOSITORIES.values()), SUITE_DENOMINATOR)

    def test_the_three_upstream_revisions_match_the_suite_lock(self) -> None:
        self.assertEqual(self.suite.dataset, "SWE-bench/SWE-bench_Multilingual")
        self.assertEqual(
            self.suite.dataset_revision, "846e647b9f33c0b51b739d005d13d85493c9af09"
        )
        self.assertEqual(
            self.suite.task_repo_revision, "6e08cbcd763c869abd7aae4bcb9e1b3991180777"
        )
        self.assertEqual(
            self.suite.grader_revision, "02e7a74ffd0b707aab73d203fe87bdc7c76afc8e"
        )
        self.assertEqual(self.suite.dataset_split, "test")
        self.assertEqual(self.suite.language, "Rust")

    def test_the_content_digest_has_not_drifted(self) -> None:
        # The drift guard. A re-cut suite is a different experiment even when
        # it still has 43 rows and the same dataset revision.
        self.assertEqual(self.suite.content_digest(), PINNED_DIGEST)
        self.assertEqual(self.suite.revisions()["instances_sha256"], PINNED_DIGEST)

    def test_every_pinned_prompt_is_real_issue_text_and_is_stored_in_full(self) -> None:
        # The problem statement is the entire prompt an episode sends. An empty
        # or truncated one would make that episode measure nothing, and the
        # adapter is forbidden from adding anything to make up the difference.
        for instance in self.suite.instances:
            with self.subTest(instance=instance.instance_id):
                self.assertTrue(instance.problem_statement.strip())
                self.assertGreater(len(instance.problem_statement), 80)
                self.assertTrue(instance.base_commit)
                self.assertIn("swebench/sweb.eval", instance.image)

    def test_the_recorded_revisions_carry_everything_a_manifest_needs(self) -> None:
        revisions = self.suite.revisions()
        for key in (
            "dataset", "dataset_revision", "dataset_split", "dataset_file",
            "dataset_file_sha256", "dataset_file_bytes", "task_repo",
            "task_repo_revision", "grader_repo", "grader_revision", "language",
            "denominator", "instance_count", "instances_sha256",
        ):
            self.assertIn(key, revisions)
        self.assertEqual(revisions["instance_count"], SUITE_DENOMINATOR)
        self.assertEqual(revisions["denominator"], SUITE_DENOMINATOR)


class ASuiteThatCouldShrinkIsRefused(unittest.TestCase):
    """Every way the denominator could quietly change is an error."""

    def setUp(self) -> None:
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        self.tmp = Path(holder.name)

    def _load(self, mutate) -> None:
        payload = _payload(Suite.load())
        mutate(payload)
        with self.assertRaises(SuiteError):
            Suite.load(_write(self.tmp, payload))

    def test_a_missing_file_names_the_path(self) -> None:
        with self.assertRaises(SuiteError) as raised:
            Suite.load(self.tmp / "absent.json")
        self.assertIn("absent.json", str(raised.exception))

    def test_a_file_that_is_not_json_is_refused(self) -> None:
        path = self.tmp / "instances.json"
        path.write_text("{not json", encoding="utf-8")
        with self.assertRaises(SuiteError):
            Suite.load(path)

    def test_an_unrecognised_schema_is_refused(self) -> None:
        self._load(lambda p: p.__setitem__("schema", "something-else/v9"))

    def test_a_dropped_instance_is_refused(self) -> None:
        # The case that matters most: 42 rows would still look like a suite and
        # would raise every mode's rate by dropping one hard task.
        self._load(lambda p: p["instances"].pop())

    def test_an_added_instance_is_refused(self) -> None:
        self._load(lambda p: p["instances"].append(dict(p["instances"][0])))

    def test_a_repeated_instance_id_is_refused(self) -> None:
        def mutate(payload: dict) -> None:
            payload["instances"][1]["instance_id"] = payload["instances"][0]["instance_id"]
        self._load(mutate)

    def test_an_unsorted_list_is_refused(self) -> None:
        def mutate(payload: dict) -> None:
            payload["instances"].reverse()
        self._load(mutate)

    def test_a_declared_denominator_that_disagrees_is_refused(self) -> None:
        self._load(lambda p: p.__setitem__("denominator", 3))

    def test_a_row_missing_a_field_names_the_field(self) -> None:
        def mutate(payload: dict) -> None:
            del payload["instances"][0]["base_commit"]
        self._load(mutate)


class SelectingASubsetCannotInventOrLoseATask(unittest.TestCase):
    """``select`` is the only way a run narrows what it attempts."""

    def setUp(self) -> None:
        self.suite = tiny_suite()

    def test_an_empty_selection_means_the_whole_suite(self) -> None:
        self.assertEqual(self.suite.select(()), self.suite.instances)

    def test_a_selection_comes_back_in_the_suites_own_order(self) -> None:
        # Two runs naming the same subset in a different order must produce
        # identical records, so the argument's order is deliberately ignored.
        forwards = self.suite.select(["aaa__one-1", "ccc__three-3"])
        backwards = self.suite.select(["ccc__three-3", "aaa__one-1"])
        self.assertEqual(forwards, backwards)
        self.assertEqual(
            [i.instance_id for i in forwards], ["aaa__one-1", "ccc__three-3"]
        )

    def test_an_unpinned_id_is_refused_rather_than_ignored(self) -> None:
        # A typo that silently shrank the run would look like a smaller sweep
        # that happened to finish faster.
        with self.assertRaises(SuiteError) as raised:
            self.suite.select(["aaa__one-1", "typo__nope-9"])
        self.assertIn("typo__nope-9", str(raised.exception))

    def test_a_subset_never_changes_the_denominator(self) -> None:
        self.assertEqual(len(self.suite.select(["aaa__one-1"])), 1)
        self.assertEqual(self.suite.denominator, SUITE_DENOMINATOR)


class TheDigestIsAFunctionOfContentAlone(unittest.TestCase):
    """What the lock does and does not react to."""

    def test_re_wording_one_problem_statement_changes_the_digest(self) -> None:
        suite = tiny_suite()
        before = instances_digest(suite.instances)
        edited = list(suite.instances)
        edited[0] = dataclasses.replace(edited[0], problem_statement="different text\n")
        self.assertNotEqual(instances_digest(edited), before)

    def test_reordering_changes_the_digest(self) -> None:
        suite = tiny_suite()
        self.assertNotEqual(
            instances_digest(tuple(reversed(suite.instances))),
            instances_digest(suite.instances),
        )

    def test_the_digest_is_stable_across_loads(self) -> None:
        self.assertEqual(Suite.load().content_digest(), Suite.load().content_digest())


if __name__ == "__main__":
    unittest.main()
