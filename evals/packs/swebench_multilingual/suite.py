"""The pinned suite: which 43 instances, at which revisions, and a lock.

``suite/instances.json`` is checked in. It carries the dataset revision, the
task-repository revision, the grader revision, the SHA-256 and byte length of
the exact dataset file those instance rows were read from, and the 43 rows
themselves — instance id, repository, base commit, version, grading image and
the ``problem_statement`` byte-for-byte.

Checking the problem statements in rather than fetching them per run is what
makes an episode auditable: the prompt the agent received is in the repository,
next to the result, and :func:`Suite.load` refuses a list whose content digest
has drifted from the one recorded in the file.
"""

from __future__ import annotations

import dataclasses
import hashlib
import json
from pathlib import Path
from typing import Iterable, Mapping, Sequence

#: The fixed denominator of the primary metric: the pinned Rust slice holds 43
#: instances and every rate is reported over 43 whatever a run attempted. It
#: lives here rather than in the core because 43 is a property of this one
#: slice; another dataset pins another number.
SUITE_DENOMINATOR = 43

#: Schema of ``suite/instances.json``.
SUITE_SCHEMA = "polaris-code-harness-eval-suite/v1"

#: The checked-in pinned list.
DEFAULT_SUITE_PATH = Path(__file__).resolve().parent / "suite" / "instances.json"


class SuiteError(RuntimeError):
    """The pinned suite is missing, malformed, or has drifted from its lock."""


@dataclasses.dataclass(frozen=True)
class Instance:
    """One pinned SWE-bench Multilingual task."""

    #: Official instance identifier, e.g. ``tokio-rs__tokio-6551``.
    instance_id: str
    #: Upstream repository, as the dataset spells it.
    repo: str
    #: The pre-solution commit the container is reset to.
    base_commit: str
    #: The dataset's version field, part of the official image name.
    version: str
    #: The official grading image for this instance.
    image: str
    #: The issue text, passed to the agent byte-for-byte and never augmented.
    problem_statement: str


@dataclasses.dataclass(frozen=True)
class Suite:
    """The pinned instance list together with the revisions it was cut from."""

    dataset: str
    dataset_revision: str
    dataset_split: str
    dataset_file: str
    dataset_file_sha256: str
    dataset_file_bytes: int
    task_repo: str
    task_repo_revision: str
    grader_repo: str
    grader_revision: str
    language: str
    repositories: tuple[str, ...]
    instances: tuple[Instance, ...]

    @property
    def denominator(self) -> int:
        """The fixed denominator of the primary metric."""
        return SUITE_DENOMINATOR

    @property
    def instance_ids(self) -> tuple[str, ...]:
        """Every pinned instance id, in the suite's canonical order."""
        return tuple(instance.instance_id for instance in self.instances)

    def content_digest(self) -> str:
        """SHA-256 over the canonical serialization of the instance rows.

        This is the lock ``recompute`` and the manifest both record: two runs
        that report the same digest attempted the same tasks with the same
        prompts, whatever else differed.
        """
        return instances_digest(self.instances)

    def select(self, instance_ids: Sequence[str]) -> tuple[Instance, ...]:
        """The instances named by ``instance_ids``, in the suite's own order.

        An empty selection means the whole suite. Order comes from the suite
        rather than from the argument so that two runs naming the same subset
        in a different order still produce identical records.

        Raises:
            SuiteError: if an id is not in the pinned list. A typo must not
                silently shrink a run.
        """
        if not instance_ids:
            return self.instances
        wanted = set(instance_ids)
        unknown = sorted(wanted - set(self.instance_ids))
        if unknown:
            raise SuiteError(
                "not in the pinned instance list: " + ", ".join(unknown)
            )
        return tuple(i for i in self.instances if i.instance_id in wanted)

    def revisions(self) -> dict[str, object]:
        """The manifest projection of everything this suite pins."""
        return {
            "schema": SUITE_SCHEMA,
            "dataset": self.dataset,
            "dataset_revision": self.dataset_revision,
            "dataset_split": self.dataset_split,
            "dataset_file": self.dataset_file,
            "dataset_file_sha256": self.dataset_file_sha256,
            "dataset_file_bytes": self.dataset_file_bytes,
            "task_repo": self.task_repo,
            "task_repo_revision": self.task_repo_revision,
            "grader_repo": self.grader_repo,
            "grader_revision": self.grader_revision,
            "language": self.language,
            "repositories": list(self.repositories),
            "denominator": self.denominator,
            "instance_count": len(self.instances),
            "instances_sha256": self.content_digest(),
        }

    @classmethod
    def load(cls, path: Path | None = None) -> "Suite":
        """Read and validate the pinned suite.

        Raises:
            SuiteError: if the file is missing, carries an unrecognised schema,
                or does not contain exactly :data:`SUITE_DENOMINATOR` instances
                with unique ids. A suite whose size drifted would silently
                change the denominator of every score derived from it.
        """
        path = path or DEFAULT_SUITE_PATH
        try:
            raw = json.loads(path.read_text(encoding="utf-8"))
        except FileNotFoundError as err:
            raise SuiteError(f"pinned suite not found at {path}") from err
        except json.JSONDecodeError as err:
            raise SuiteError(f"pinned suite at {path} is not valid JSON: {err}") from err

        schema = raw.get("schema")
        if schema != SUITE_SCHEMA:
            raise SuiteError(
                f"pinned suite at {path} has schema {schema!r}, expected {SUITE_SCHEMA!r}"
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
            raise SuiteError(
                f"pinned suite at {path} is not in ascending instance-id order"
            )

        declared = raw.get("denominator")
        if declared != SUITE_DENOMINATOR:
            raise SuiteError(
                f"pinned suite at {path} declares denominator {declared!r}, "
                f"expected {SUITE_DENOMINATOR}"
            )

        return cls(
            dataset=str(raw["dataset"]),
            dataset_revision=str(raw["dataset_revision"]),
            dataset_split=str(raw["dataset_split"]),
            dataset_file=str(raw["dataset_file"]),
            dataset_file_sha256=str(raw["dataset_file_sha256"]),
            dataset_file_bytes=int(raw["dataset_file_bytes"]),
            task_repo=str(raw["task_repo"]),
            task_repo_revision=str(raw["task_repo_revision"]),
            grader_repo=str(raw["grader_repo"]),
            grader_revision=str(raw["grader_revision"]),
            language=str(raw["language"]),
            repositories=tuple(str(r) for r in raw["repositories"]),
            instances=instances,
        )


def _instance(row: Mapping[str, object], path: Path) -> Instance:
    """Build one :class:`Instance`, naming the file in any failure."""
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
    """SHA-256 over the canonical serialization of ``instances``."""
    payload = json.dumps(
        [dataclasses.asdict(instance) for instance in instances],
        sort_keys=True,
        ensure_ascii=False,
    )
    return hashlib.sha256(payload.encode("utf-8")).hexdigest()
