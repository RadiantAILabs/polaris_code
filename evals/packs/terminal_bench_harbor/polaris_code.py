"""Run the shipped ``polaris-code`` binary as a Harbor installed agent.

The adapter is a Terminal-Bench/Harbor integration, not a dataset pack for
``evals.core``.  Harbor owns its jobs, trial records, environments and
verifiers, while ``polaris-code`` runs inside the task container and leaves its
answer in that container's final filesystem state.

The host binary is deliberately an explicit Harbor agent kwarg.  It is copied
to the task during setup; no build tree, source checkout, or credential is
copied.  Provider credentials are not read here.  Harbor supplies them only as
agent environment variables scoped to setup and execution.
"""

from __future__ import annotations

import hashlib
import shlex
from pathlib import Path
from typing import Any, Sequence, override

from harbor.agents.installed.base import BaseInstalledAgent
from harbor.agents.model_connection import ModelConnectionSpec
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext


#: The non-root-readable location used by every task image after setup.
REMOTE_BINARY = "/installed-agent/polaris-code"


def _command_patterns(value: Sequence[str] | str | None) -> tuple[str, ...]:
    """Normalize the explicit unattended shell grants from Harbor kwargs."""
    if value is None:
        return ()
    if isinstance(value, str):
        return (value,)
    return tuple(value)


class PolarisCode(BaseInstalledAgent):
    """A portable ``polaris-code`` binary installed into a Harbor task image.

    Pass ``binary=/absolute/path/to/polaris-code`` through Harbor's
    ``--agent-kwarg`` option.  ``allow_command`` is intentionally opt-in: a
    caller must give the same explicit shell-pattern authority that the native
    unattended CLI requires.
    """

    # Polaris reads the provider's normal environment variable itself.  Harbor
    # only needs to preserve that environment, so it must not rewrite or read a
    # provider key through ModelConnectionSpec.
    MODEL_CONNECTION = ModelConnectionSpec(passthrough=True)

    def __init__(
        self,
        logs_dir: Path,
        binary: str | Path,
        allow_command: Sequence[str] | str | None = None,
        graph_search: str = "both",
        *args: Any,
        **kwargs: Any,
    ) -> None:
        binary_path = Path(binary).expanduser().resolve()
        if not binary_path.is_file():
            raise FileNotFoundError(f"polaris-code binary is not a file: {binary_path}")
        self._binary = binary_path
        self._binary_sha256 = hashlib.sha256(binary_path.read_bytes()).hexdigest()
        self._allow_command = _command_patterns(allow_command)
        self._graph_search = graph_search
        super().__init__(logs_dir, *args, version=f"sha256:{self._binary_sha256}", **kwargs)

    @staticmethod
    @override
    def name() -> str:
        """Return Harbor's stable display name for this custom agent."""
        return "polaris-code"

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        """Upload the already-built binary and make it executable to the agent."""
        await environment.upload_file(str(self._binary), REMOTE_BINARY)
        # upload_file is host-to-container transfer and may preserve a restrictive
        # host mode.  Harbor's task agent is not root, so setup fixes the mode.
        await self.exec_as_root(environment, command=f"chmod 755 {REMOTE_BINARY}")

    @override
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        """Run one non-interactive turn in the image's own declared directory."""
        if not self.model_name:
            raise ValueError("polaris-code Harbor runs require Harbor --model")

        argv = [
            REMOTE_BINARY,
            "-p",
            instruction,
            "--output-format",
            "stream-json",
            "--model",
            self.model_name,
            "--allow-edit",
        ]
        for pattern in self._allow_command:
            argv.extend(("--allow-command", pattern))

        # Do not set a cwd or a positional working directory: Terminal-Bench
        # task images deliberately use different Dockerfile WORKDIR values.
        # Harbor's exec preserves that task-defined directory, which is the
        # only portable working-tree boundary across all 89 tasks.
        await self.exec_as_agent(
            environment,
            command=shlex.join(argv),
            env={"POLARIS_CODE_GRAPH_SEARCH": self._graph_search},
        )
