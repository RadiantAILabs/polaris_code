#!/usr/bin/env python3
"""Dispatch a published coding-agent release to the Homebrew tap."""

import json
import os
from pathlib import Path
import re
import urllib.error
import urllib.request


DISPATCH_URL = (
    "https://api.github.com/repos/RadiantAILabs/homebrew-tap/"
    "actions/workflows/tests.yml/dispatches"
)
WORKFLOW_URL = "https://github.com/RadiantAILabs/homebrew-tap/actions/workflows/tests.yml"


def payload(tag: str, force_rebuild: bool) -> dict:
    """Build the target workflow's inputs for a stable, versioned release."""
    if not re.fullmatch(r"v\d+\.\d+\.\d+", tag):
        raise ValueError("Homebrew releases require a stable vMAJOR.MINOR.PATCH tag")
    return {
        "ref": "main",
        "inputs": {
            "release_tag": tag,
            "force_rebuild": str(force_rebuild).lower(),
        },
    }


def dispatch(tag: str, token: str, force_rebuild: bool = False, opener=None) -> str:
    """Trigger the tap using its dedicated Actions token, returning a run link."""
    body = payload(tag, force_rebuild)
    if not token:
        raise ValueError("Configure the HOMEBREW_TAP_TOKEN Actions secret before releasing")
    request = urllib.request.Request(
        DISPATCH_URL,
        data=json.dumps(body).encode(),
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": "Bearer " + token,
            "Content-Type": "application/json",
            "User-Agent": "polaris-code-homebrew-release",
            "X-GitHub-Api-Version": "2026-03-10",
        },
        method="POST",
    )
    with (opener or urllib.request.urlopen)(request, timeout=30) as response:
        result = response.read()
    return json.loads(result).get("html_url", WORKFLOW_URL) if result else WORKFLOW_URL


def main() -> None:
    """Dispatch the release selected by the GitHub workflow context."""
    tag = os.environ.get("RELEASE_TAG", "")
    try:
        url = dispatch(
            tag,
            os.environ.get("HOMEBREW_TAP_TOKEN", ""),
            os.environ.get("FORCE_REBUILD", "false") == "true",
        )
    except urllib.error.HTTPError as error:
        raise SystemExit(
            f"Homebrew dispatch returned HTTP {error.code}; check the token's tap Actions permission"
        ) from None
    except (ValueError, urllib.error.URLError) as error:
        raise SystemExit(str(error)) from None
    print(f"Triggered Homebrew for {tag}: {url}")
    if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        with Path(summary).open("a") as output:
            output.write(f"Homebrew rebuild for **{tag}**: [workflow run]({url}).\n")


if __name__ == "__main__":
    main()
