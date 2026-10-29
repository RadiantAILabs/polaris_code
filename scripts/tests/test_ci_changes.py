"""Check CI gating against real Git histories, including whole-push and PR ranges."""

import importlib.util
import subprocess
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "ci_changes.py"
SPEC = importlib.util.spec_from_file_location("ci_changes", SCRIPT)
ci_changes = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ci_changes)


class CiChangesTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.repository = Path(self.temporary.name)
        self.git("init", "--quiet", "-b", "main")
        self.git("config", "user.name", "CI fixture")
        self.git("config", "user.email", "ci@example.invalid")
        self.git("config", "commit.gpgsign", "false")
        self.base = self.commit("README.md", "initial\n")

    def git(self, *arguments):
        return subprocess.check_output(
            ["git", *arguments], cwd=self.repository, text=True
        ).strip()

    def commit(self, path, content):
        file = self.repository / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(content)
        self.git("add", "--all")
        self.git("commit", "--quiet", "-m", "Update fixture")
        return self.git("rev-parse", "HEAD")

    def push_requires_ci(self, head, base=None):
        return ci_changes.requires_full_ci(
            "push", {"before": base or self.base, "after": head},
            "refs/heads/main", self.repository,
        )

    def test_documentation_and_branding_skip_full_ci(self):
        self.commit("README.md", "updated documentation\n")
        self.commit("docs/publishing.md", "release instructions\n")
        self.commit("crates/polaris_code/README.md", "package documentation\n")
        self.commit("evals/README.md", "evaluation instructions\n")
        self.commit("evals/results/report.md", "evaluation report\n")
        head = self.commit("assets/polaris-logo.svg", "<svg/>\n")
        self.assertFalse(self.push_requires_ci(head))

    def test_source_change_in_earlier_push_commit_is_not_missed(self):
        self.commit("crates/polaris_code/src/main.rs", "fn main() {}\n")
        head = self.commit("README.md", "follow-up documentation\n")
        self.assertTrue(self.push_requires_ci(head))

    def test_pr_uses_merge_base_and_ignores_changes_only_on_target_branch(self):
        self.git("checkout", "--quiet", "-b", "feature")
        head = self.commit("README.md", "PR documentation\n")
        self.git("checkout", "--quiet", "main")
        base = self.commit("crates/polaris_code/src/main.rs", "fn main() {}\n")
        event = {"pull_request": {"base": {"sha": base}, "head": {"sha": head}}}
        self.assertFalse(ci_changes.requires_full_ci(
            "pull_request", event, "refs/pull/1/merge", self.repository,
        ))

    def test_deleted_source_still_requires_full_ci(self):
        base = self.commit("crates/polaris_code/src/main.rs", "fn main() {}\n")
        self.git("mv", "crates/polaris_code/src/main.rs", "example.txt")
        self.git("commit", "--quiet", "-m", "Move fixture source")
        self.assertTrue(self.push_requires_ci(self.git("rev-parse", "HEAD"), base))

    def test_missing_comparison_requires_full_ci(self):
        self.assertTrue(self.push_requires_ci(self.base, "1" * 40))
        self.assertTrue(self.push_requires_ci(self.base, "0" * 40))

    def test_tags_and_manual_dispatch_always_run_full_ci(self):
        for event_name, ref in (
            ("push", "refs/tags/v0.1.1"),
            ("workflow_dispatch", "refs/heads/main"),
        ):
            with self.subTest(event_name=event_name):
                self.assertTrue(ci_changes.requires_full_ci(
                    event_name, {}, ref, self.repository,
                ))

    def test_dependencies_tooling_and_embedded_docs_require_full_ci(self):
        for path in (
            "Cargo.lock", ".cargo/config.toml", "rust-toolchain.toml",
            "crates/polaris_code/Cargo.toml", "crates/polaris_code/build.rs",
            "crates/polaris_code/src/docs/embedding.md",
            "crates/polaris_code/tests/fixture.json",
            "crates/polaris_ontrace/ontologies/core.ttl",
            "evals/core/runner.py", "evals/packs/swebench_verified/suite/instances.json",
            "evals/tasks/rs-add-test/task.md",
            ".github/workflows/ci.yml", "scripts/homebrew_formula.py",
            "scripts/ci_changes.py", "scripts/tests/test_ci_changes.py",
            "scripts/tests/test_homebrew_formula.py",
        ):
            with self.subTest(path=path):
                self.assertTrue(ci_changes.affects_build(path))


if __name__ == "__main__":
    unittest.main()
