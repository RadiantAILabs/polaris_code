"""Install and run Polaris in Harbor's task environment.

Use a pinned GitHub release for reproducible evaluations, or upload an explicit
local executable for development. Credentials stay in Harbor's agent environment.
Harbor downloads the native ATIF trajectory before populating the run context.
"""

from __future__ import annotations

import hashlib
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


def release_install_command(version: str) -> str:
    """Download the Linux release for the sandbox CPU and verify its checksum."""
    version = version.removeprefix("v")
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", version):
        raise ValueError("version must pin a release, for example 0.3.0 or v0.3.0")
    return f'''set -eu
[ "$(uname -s)" = Linux ] || {{ echo 'Polaris release installation requires Linux' >&2; exit 1; }}
case "$(uname -m)" in
  x86_64|amd64) target=x86_64-unknown-linux-gnu ;;
  aarch64|arm64) target=aarch64-unknown-linux-gnu ;;
  *) echo 'Unsupported Polaris release architecture' >&2; exit 1 ;;
esac
archive="polaris-code-{version}-$target.tar.gz"
release_tmp=$(mktemp -d)
trap 'rm -rf "$release_tmp"' EXIT
cd "$release_tmp"
curl -fL --retry 3 "{RELEASE_URL}/v{version}/$archive" -o "$archive"
curl -fL --retry 3 "{RELEASE_URL}/v{version}/$archive.sha256" -o "$archive.sha256"
sha256sum -c "$archive.sha256"
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
        or int(value) <= 0
    ):
        raise ValueError(f"{name} must be a positive integer")
    return int(value)


class PolarisCode(BaseInstalledAgent):
    """Harbor installed agent using either ``version`` or a local ``binary``.

    ``unattended=True`` explicitly grants edits and shell execution inside the
    disposable sandbox. Otherwise only edits and ``allow_command`` patterns are
    granted. Model credentials are supplied through Harbor's ``extra_env``.
    """

    capabilities = AgentCapabilities(atif=True)
    MODEL_CONNECTION = ModelConnectionSpec(passthrough=True)

    def __init__(
        self,
        logs_dir: Path,
        binary: str | Path | None = None,
        version: str | None = None,
        allow_command: Sequence[str] | str | None = None,
        graph_search: str = "both",
        provider: str | None = None,
        base_url: str | None = None,
        reasoning_effort: str | None = None,
        context_window: int | str | None = None,
        max_output_tokens: int | str | None = None,
        unattended: bool | str = False,
        *args: Any,
        **kwargs: Any,
    ) -> None:
        if (binary is None) == (version is None):
            raise ValueError("supply exactly one of binary or a pinned version")
        self._binary = None
        if binary is not None:
            binary_path = Path(binary).expanduser().resolve()
            if not binary_path.is_file():
                raise FileNotFoundError(f"Polaris binary is not a file: {binary_path}")
            self._binary = binary_path
            version = f"sha256:{hashlib.sha256(binary_path.read_bytes()).hexdigest()}"
        else:
            assert version is not None
            release_install_command(version)  # Validate before any environment spend.
            version = version.removeprefix("v")
        if graph_search not in ("curated", "freeform", "both"):
            raise ValueError("graph_search must be curated, freeform, or both")
        self._allow_command = (
            (allow_command,)
            if isinstance(allow_command, str)
            else tuple(allow_command or ())
        )
        self._graph_search = graph_search
        self._provider = provider
        self._base_url = base_url
        self._reasoning_effort = reasoning_effort
        self._context_window = _positive_int("context_window", context_window)
        self._max_output_tokens = _positive_int("max_output_tokens", max_output_tokens)
        self._unattended = parse_bool_env_value(unattended, name="unattended")
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
            await self.exec_as_root(
                environment, command=release_install_command(self._version)
            )

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
            "-p",
            instruction,
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
        # Preserve the task image's declared WORKDIR as the containment boundary.
        await self.exec_as_agent(
            environment,
            command=f"{shlex.join(argv)} </dev/null >{shlex.quote(logs + '/polaris.jsonl')} 2>{shlex.quote(logs + '/polaris-stderr.txt')}",
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
