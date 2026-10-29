"""Policy facts specific to the SWE-bench Verified dataset pin."""

from __future__ import annotations

from typing import Any


def policy() -> dict[str, Any]:
    """Return the policy recorded with every Verified evaluation manifest.

    The upstream card identifies this as its human-validated 500-instance test
    split.  Its problem-text redistribution terms are not established here, so
    the pack records that fact rather than asserting a licence it has not
    verified.
    """
    return {
        "split": "test",
        "split_is_the_only_one_used": True,
        "language_filter": "Python",
        "held_out_split": None,
        "problem_statement_checked_in": True,
        "problem_statement_licence": None,
        "problem_statement_licence_note": (
            "Refer to the upstream dataset for problem-text redistribution "
            "terms."
        ),
        "images_are_upstream": True,
        "grading_is_official": True,
        "human_validated": True,
    }
