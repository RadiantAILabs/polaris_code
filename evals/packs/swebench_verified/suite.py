"""The immutable 500-instance SWE-bench Verified suite and its lock."""

from __future__ import annotations

import dataclasses
import hashlib
import json
from pathlib import Path
from typing import Iterable, Mapping, Sequence


#: The official human-validated Verified test set size.
SUITE_DENOMINATOR = 500
#: Schema of the checked-in suite lock.
SUITE_SCHEMA = "polaris-code-harness-eval-suite/v1"
#: The checked-in suite lock.
DEFAULT_SUITE_PATH = Path(__file__).resolve().parent / "suite" / "instances.json"


class SuiteError(RuntimeError):
    """The pinned suite is missing, malformed, or has drift from its lock."""


@dataclasses.dataclass(frozen=True)
class Instance:
    """One official SWE-bench Verified task."""

    instance_id: str
    repo: str
    base_commit: str
    version: str
    image: str
    problem_statement: str


@dataclasses.dataclass(frozen=True)
class GoldPatch:
    """One upstream reference patch used only to verify the evaluator path."""

    instance_id: str
    model_patch: str


@dataclasses.dataclass(frozen=True)
class Suite:
    """The official dataset pin and the 500 task rows it contains."""

    dataset: str
    dataset_revision: str
    dataset_split: str
    dataset_file: str
    dataset_file_sha256: str
    dataset_file_bytes: int
    grader_repo: str
    grader_revision: str
    language: str
    repositories: tuple[str, ...]
    instances: tuple[Instance, ...]
    gold: GoldPatch

    @property
    def denominator(self) -> int:
        """The unchanging denominator used for Verified measurements."""
        return SUITE_DENOMINATOR

    @property
    def instance_ids(self) -> tuple[str, ...]:
        """Every instance id in canonical order."""
        return tuple(instance.instance_id for instance in self.instances)

    def content_digest(self) -> str:
        """Return the canonical SHA-256 over the agent-visible task rows."""
        return instances_digest(self.instances)

    def select(self, instance_ids: Sequence[str]) -> tuple[Instance, ...]:
        """Select named instances in suite order, or all when none are named."""
        if not instance_ids:
            return self.instances
        wanted = set(instance_ids)
        unknown = sorted(wanted - set(self.instance_ids))
        if unknown:
            raise SuiteError("not in the pinned instance list: " + ", ".join(unknown))
        return tuple(instance for instance in self.instances if instance.instance_id in wanted)

    def revisions(self) -> dict[str, object]:
        """Project all upstream inputs that define this suite."""
        return {
            "schema": SUITE_SCHEMA,
            "dataset": self.dataset,
            "dataset_revision": self.dataset_revision,
            "dataset_split": self.dataset_split,
            "dataset_file": self.dataset_file,
            "dataset_file_sha256": self.dataset_file_sha256,
            "dataset_file_bytes": self.dataset_file_bytes,
            "grader_repo": self.grader_repo,
            "grader_revision": self.grader_revision,
            "language": self.language,
            "repositories": list(self.repositories),
            "denominator": self.denominator,
            "instance_count": len(self.instances),
            "instances_sha256": self.content_digest(),
            "gold_instance_id": self.gold.instance_id,
            "gold_patch_sha256": hashlib.sha256(
                self.gold.model_patch.encode("utf-8")
            ).hexdigest(),
        }

    @classmethod
    def load(cls, path: Path | None = None) -> "Suite":
        """Read and validate the checked-in official dataset pin."""
        path = path or DEFAULT_SUITE_PATH
        try:
            raw = json.loads(path.read_text(encoding="utf-8"))
        except FileNotFoundError as err:
            raise SuiteError(f"pinned suite not found at {path}") from err
        except json.JSONDecodeError as err:
            raise SuiteError(f"pinned suite at {path} is not valid JSON: {err}") from err
        if raw.get("schema") != SUITE_SCHEMA:
            raise SuiteError(
                f"pinned suite at {path} has schema {raw.get('schema')!r}, "
                f"expected {SUITE_SCHEMA!r}"
            )
        instances = tuple(_instance(row, path) for row in raw.get("instances", ()))
        if len(instances) != SUITE_DENOMINATOR:
            raise SuiteError(
                f"pinned suite at {path} holds {len(instances)} instances, "
                f"expected exactly {SUITE_DENOMINATOR}"
            )
        ids = [instance.instance_id for instance in instances]
        if len(set(ids)) != len(ids):
            raise SuiteError(f"pinned suite at {path} repeats an instance id")
        if ids != sorted(ids):
            raise SuiteError(f"pinned suite at {path} is not in ascending instance-id order")
        if raw.get("denominator") != SUITE_DENOMINATOR:
            raise SuiteError(
                f"pinned suite at {path} declares denominator {raw.get('denominator')!r}, "
                f"expected {SUITE_DENOMINATOR}"
            )
        try:
            gold_raw = raw["gold"]
            gold = GoldPatch(
                instance_id=str(gold_raw["instance_id"]),
                model_patch=str(gold_raw["model_patch"]),
            )
        except (KeyError, TypeError) as err:
            raise SuiteError(f"pinned suite at {path} has no valid gold patch") from err
        if gold.instance_id not in ids:
            raise SuiteError(f"gold patch instance {gold.instance_id!r} is not in the suite")
        return cls(
            dataset=str(raw["dataset"]),
            dataset_revision=str(raw["dataset_revision"]),
            dataset_split=str(raw["dataset_split"]),
            dataset_file=str(raw["dataset_file"]),
            dataset_file_sha256=str(raw["dataset_file_sha256"]),
            dataset_file_bytes=int(raw["dataset_file_bytes"]),
            grader_repo=str(raw["grader_repo"]),
            grader_revision=str(raw["grader_revision"]),
            language=str(raw["language"]),
            repositories=tuple(str(repo) for repo in raw["repositories"]),
            instances=instances,
            gold=gold,
        )


def _instance(row: Mapping[str, object], path: Path) -> Instance:
    """Parse one locked task row, retaining the file name on failure."""
    try:
        return Instance(
            instance_id=str(row["instance_id"]),
            repo=str(row["repo"]),
            base_commit=str(row["base_commit"]),
            version=str(row["version"]),
            image=str(row["image"]),
            problem_statement=str(row["problem_statement"]),
        )
    except KeyError as err:
        raise SuiteError(f"instance row in {path} is missing {err}") from err


def instances_digest(instances: Iterable[Instance]) -> str:
    """Return the canonical digest over the checked-in task rows."""
    payload = json.dumps(
        [dataclasses.asdict(instance) for instance in instances],
        sort_keys=True,
        ensure_ascii=False,
    )
    return hashlib.sha256(payload.encode("utf-8")).hexdigest()
