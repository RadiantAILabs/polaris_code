"""Install and run Polaris in Harbor's task environment.

Use a pinned GitHub release for reproducible evaluations, or upload an explicit
local executable for development. Credentials stay in Harbor's agent environment.
Harbor downloads the native ATIF trajectory before populating the run context.
"""

from __future__ import annotations

import hashlib
import json
import math
import os
import re
import shlex
from collections.abc import Sequence
from pathlib import Path
from typing import Any, override

from harbor.agents.capabilities import AgentCapabilities
from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.agents.model_connection import ModelConnectionSpec
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext
from harbor.models.trajectories import Trajectory
from harbor.utils.env import parse_bool_env_value

REMOTE_BINARY = "/installed-agent/polaris-code"
RELEASE_URL = "https://github.com/RadiantAILabs/polaris_code/releases/download"
TRAJECTORY = "trajectory.json"
STREAM_LOG = "polaris.jsonl"
STDERR_LOG = "polaris-stderr.txt"
# The usage event is the last stdout line, so only the end of the log is read.
MAX_STREAM_BYTES = 8 * 1024 * 1024
# The CLI takes the context window and output limit as unsigned 32-bit integers.
MAX_MODEL_LIMIT = 2**32 - 1


def release_install_command(version: str, sha256: str | None = None) -> str:
    """Download the static Linux x86-64 release and verify its checksum.

    ``sha256`` is the expected digest of the archive. Without it the script
    trusts the ``.sha256`` file of the same release, which detects corruption
    but not a replaced release. The file must name the downloaded archive.
    """
    pin = version.removeprefix("v")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?", pin):
        raise ValueError(
            f"version must pin a release, for example 0.3.0 or v0.3.0, got {version!r}"
        )
    base = f"{RELEASE_URL}/v{pin}"
    curl = "curl -fL --proto '=https' --proto-redir '=https' --retry 3"
    if sha256 is None:
        checksum = (
            f'{curl} "{base}/$archive.sha256" -o "$archive.sha256"\n'
            'read -r listed_digest listed_name < "$archive.sha256" || true\n'
            '[ "${listed_name#\\*}" = "$archive" ] || '
            '{ echo "$archive.sha256 does not name $archive" >&2; exit 1; }\n'
            'echo "$listed_digest  $archive" | sha256sum -c -'
        )
    elif re.fullmatch(r"[0-9a-fA-F]{64}", sha256):
        checksum = f'echo "{sha256.lower()}  $archive" | sha256sum -c -'
    else:
        raise ValueError("sha256 must be 64 hexadecimal digits")
    return f'''set -eu
[ "$(uname -s)" = Linux ] || {{ echo 'Polaris release installation requires Linux' >&2; exit 1; }}
case "$(uname -m)" in
  x86_64|amd64) target=x86_64-unknown-linux-musl ;;
  *) echo 'Unsupported CPU: Polaris release installation supports x86_64-unknown-linux-musl only' >&2; exit 1 ;;
esac
archive="polaris-code-{pin}-$target.tar.gz"
release_tmp=$(mktemp -d)
trap 'rm -rf "$release_tmp"' EXIT
cd "$release_tmp"
{curl} "{base}/$archive" -o "$archive"
{checksum}
tar -xzf "$archive" polaris
cp polaris {REMOTE_BINARY}
chmod 755 {REMOTE_BINARY}
{REMOTE_BINARY} --version'''


def _positive_int(name: str, value: int | str | None) -> int | None:
    if value is None:
        return None
    if (
        isinstance(value, bool)
        or not re.fullmatch(r"[0-9]+", str(value))
        or not 0 < int(value) <= MAX_MODEL_LIMIT
    ):
        raise ValueError(f"{name} must be a positive integer up to {MAX_MODEL_LIMIT}")
    return int(value)


def _reported_cost(logs_dir: Path) -> float | None:
    """Read a known cost from the application's authoritative usage event.

    The sandbox writes this file, so no content in it may raise: undecodable
    bytes are replaced, each line is parsed alone, and only the last
    ``MAX_STREAM_BYTES`` are read.
    """
    try:
        with (logs_dir / STREAM_LOG).open("rb") as stream:
            stream.seek(0, os.SEEK_END)
            stream.seek(max(0, stream.tell() - MAX_STREAM_BYTES))
            tail = stream.read(MAX_STREAM_BYTES)
    except OSError:
        return None
    cost = None
    for line in tail.decode("utf-8", errors="replace").split("\n"):
        try:
            event = json.loads(line)
            if (
                event.get("source") != "System"
                or event.get("metadata", {}).get("io_type") != "usage"
            ):
                continue
            usage = json.loads(event["content"]["Text"])
            if usage.get("schema") != "polaris-code-run-usage/v1":
                continue
            value = usage.get("estimated_cost_usd")
            if (
                isinstance(value, (int, float))
                and not isinstance(value, bool)
                and math.isfinite(value)
                and value >= 0
            ):
                cost = float(value)
        except (
            ValueError,
            KeyError,
            TypeError,
            AttributeError,
            OverflowError,
            RecursionError,
        ):
            continue
    return cost


class PolarisCode(BaseInstalledAgent):
    """Harbor installed agent using either ``version`` or a local ``binary``.

    The keywords are documented in ``docs/terminal-bench-harbor.md``.
    ``unattended=True`` passes ``--dangerously-skip-permissions``: the model may
    edit any file and run any command, so the task container is the only
    boundary. Otherwise only edits and ``allow_command`` patterns are granted;
    the two cannot be combined. Model credentials are supplied through Harbor's
    ``extra_env``.
    """

    capabilities = AgentCapabilities(atif=True)
    MODEL_CONNECTION = ModelConnectionSpec(passthrough=True)

    def __init__(
        self,
        logs_dir: Path,
        *args: Any,
        binary: str | Path | None = None,
        version: str | None = None,
        sha256: str | None = None,
        allow_command: Sequence[str] | str | None = None,
        graph_search: str = "both",
        provider: str | None = None,
        base_url: str | None = None,
        reasoning_effort: str | None = None,
        context_window: int | str | None = None,
        max_output_tokens: int | str | None = None,
        unattended: bool | str = False,
        **kwargs: Any,
    ) -> None:
        if (binary is None) == (version is None):
            raise ValueError("supply exactly one of binary or a pinned version")
        self._binary = None
        self._sha256 = sha256
        if binary is not None:
            if sha256 is not None:
                raise ValueError("sha256 pins a release; use it with version")
            binary_path = Path(binary).expanduser().resolve()
            if not binary_path.is_file():
                raise FileNotFoundError(f"Polaris binary is not a file: {binary_path}")
            self._binary = binary_path
            version = f"sha256:{hashlib.sha256(binary_path.read_bytes()).hexdigest()}"
        else:
            assert version is not None
            # Validate before any environment spend.
            release_install_command(version, sha256)
            version = version.removeprefix("v")
        if graph_search not in ("curated", "freeform", "both"):
            raise ValueError("graph_search must be curated, freeform, or both")
        self._allow_command = (
            (allow_command,)
            if isinstance(allow_command, str)
            else tuple(allow_command or ())
        )
        self._unattended = parse_bool_env_value(unattended, name="unattended")
        if self._unattended and self._allow_command:
            # The CLI rejects this pair, but only after the environment is built.
            raise ValueError(
                "allow_command cannot be combined with unattended; "
                "unattended already allows every command"
            )
        self._graph_search = graph_search
        self._provider = provider
        self._base_url = base_url
        self._reasoning_effort = reasoning_effort
        self._context_window = _positive_int("context_window", context_window)
        self._max_output_tokens = _positive_int("max_output_tokens", max_output_tokens)
        if (
            self._context_window is not None
            and self._max_output_tokens is not None
            and self._context_window <= self._max_output_tokens + 1
        ):
            raise ValueError(
                "context_window must exceed max_output_tokens by at least two tokens"
            )
        super().__init__(logs_dir, *args, version=version, **kwargs)

    @staticmethod
    @override
    def name() -> str:
        """Return Harbor's stable agent name."""
        return "polaris-code"

    @override
    def get_version_command(self) -> str:
        """Return the installed executable's version command."""
        return f"{REMOTE_BINARY} --version"

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        """Install only the chosen executable, never a source or credential tree."""
        if self._binary is not None:
            await environment.upload_file(str(self._binary), REMOTE_BINARY)
            await self.exec_as_root(environment, command=f"chmod 755 {REMOTE_BINARY}")
        else:
            await self.ensure_system_dependencies(environment, ("curl", "tar"))
            command = release_install_command(self._version, self._sha256)
            await self.exec_as_root(environment, command=command)

    @override
    @with_prompt_template
    async def run(
        self, instruction: str, environment: BaseEnvironment, context: AgentContext
    ) -> None:
        """Run in the image's WORKDIR and write native ATIF to Harbor's logs."""
        if not self.model_name:
            raise ValueError("Polaris Harbor runs require Harbor --model")
        logs = str(self.environment_logs_dir)
        argv = [
            REMOTE_BINARY,
            # The joined form keeps an instruction that starts with "-" a value.
            f"--print={instruction}",
            "--output-format",
            "stream-json",
            "--model",
            self.model_name,
            "--trajectory-path",
            f"{logs}/{TRAJECTORY}",
        ]
        argv.append(
            "--dangerously-skip-permissions" if self._unattended else "--allow-edit"
        )
        for flag, value in (
            ("--provider", self._provider),
            ("--reasoning-effort", self._reasoning_effort),
            ("--context-window", self._context_window),
            ("--max-output-tokens", self._max_output_tokens),
        ):
            if value is not None:
                argv.extend((flag, str(value)))
        for pattern in self._allow_command:
            argv.extend(("--allow-command", pattern))
        stdout_log = shlex.quote(f"{logs}/{STREAM_LOG}")
        stderr_log = shlex.quote(f"{logs}/{STDERR_LOG}")
        # The run starts in the task image's declared WORKDIR. With
        # --dangerously-skip-permissions that is only the start directory: the
        # container is the one boundary.
        await self.exec_as_agent(
            environment,
            command=f"{shlex.join(argv)} </dev/null >{stdout_log} 2>{stderr_log}",
            env={
                "POLARIS_CODE_GRAPH_SEARCH": self._graph_search,
                **(
                    {"OPENAI_BASE_URL": self._base_url}
                    if self._base_url is not None
                    else {}
                ),
            },
        )

    @override
    def populate_context_post_run(self, context: AgentContext) -> None:
        """Read validated native ATIF after Harbor downloads the agent logs."""
        path = self.logs_dir / TRAJECTORY
        try:
            trajectory = Trajectory.model_validate_json(path.read_text())
        except (OSError, ValueError):
            self.logger.warning(
                "Polaris ATIF trajectory is missing or invalid", exc_info=True
            )
            return
        metrics = trajectory.final_metrics
        if metrics is not None:
            context.n_input_tokens = metrics.total_prompt_tokens
            context.n_cache_tokens = metrics.total_cached_tokens
            context.n_output_tokens = metrics.total_completion_tokens
            context.cost_usd = metrics.total_cost_usd
        if context.cost_usd is None:
            context.cost_usd = _reported_cost(self.logs_dir)
