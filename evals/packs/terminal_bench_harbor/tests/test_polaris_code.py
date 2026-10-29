"""The installed-agent boundary is tested without a container or provider key."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

try:
    from harbor.models.agent.context import AgentContext

    from ..polaris_code import PolarisCode, REMOTE_BINARY

    HARBOR_INSTALLED = True
except ImportError:
    HARBOR_INSTALLED = False


class RecordingEnvironment:
    """Minimal Harbor environment recording the calls an installed agent makes."""

    default_user = "agent"

    def __init__(self) -> None:
        self.uploads: list[tuple[str, str]] = []
        self.execs: list[dict[str, object]] = []

    async def upload_file(self, source_path: str, target_path: str) -> None:
        self.uploads.append((source_path, target_path))

    async def exec(self, **kwargs: object) -> SimpleNamespace:
        self.execs.append(kwargs)
        return SimpleNamespace(return_code=0, stdout="", stderr="")


@unittest.skipUnless(HARBOR_INSTALLED, "the optional 'harbor' package is not installed")
class PolarisCodeInstalledAgent(unittest.IsolatedAsyncioTestCase):
    """The adapter copies a binary, then invokes one safe unattended turn."""

    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.binary = Path(self.directory.name) / "polaris-code"
        self.binary.write_bytes(b"portable binary fixture")
        self.logs = Path(self.directory.name) / "logs"

    def tearDown(self) -> None:
        self.directory.cleanup()

    def _agent(self, **kwargs: object) -> PolarisCode:
        return PolarisCode(self.logs, binary=self.binary, model_name="anthropic/test", **kwargs)

    async def test_setup_uploads_only_the_selected_binary_then_grants_execute_mode(self) -> None:
        environment = RecordingEnvironment()
        agent = self._agent()

        await agent.setup(environment)  # type: ignore[arg-type]

        self.assertEqual(environment.uploads, [(str(self.binary), REMOTE_BINARY)])
        commands = [str(call["command"]) for call in environment.execs]
        self.assertTrue(
            any(command.endswith("chmod 755 /installed-agent/polaris-code") for command in commands)
        )
        self.assertNotIn("ANTHROPIC_API_KEY", "\n".join(commands))

    async def test_turn_has_the_original_instruction_model_and_only_explicit_shell_grants(self) -> None:
        environment = RecordingEnvironment()
        agent = self._agent(allow_command=("pytest *",), graph_search="curated")
        instruction = "Fix it; quotes ' and $(not shell) stay data."

        await agent.run(instruction, environment, AgentContext())  # type: ignore[arg-type]

        call = environment.execs[-1]
        command = str(call["command"])
        self.assertIn("--model anthropic/test", command)
        self.assertIn("--allow-edit", command)
        self.assertIn("--allow-command 'pytest *'", command)
        self.assertIn("Fix it; quotes", command)
        self.assertIn("$(not shell) stay data.", command)
        self.assertEqual(call["env"], {"POLARIS_CODE_GRAPH_SEARCH": "curated"})
        self.assertNotIn("ANTHROPIC_API_KEY", command)

    async def test_model_is_required_before_the_binary_is_started(self) -> None:
        environment = RecordingEnvironment()
        agent = PolarisCode(self.logs, binary=self.binary)

        with self.assertRaisesRegex(ValueError, "Harbor --model"):
            await agent.run("x", environment, AgentContext())  # type: ignore[arg-type]
        self.assertEqual(environment.execs, [])

    def test_missing_binary_is_refused_at_adapter_construction(self) -> None:
        with self.assertRaisesRegex(FileNotFoundError, "not a file"):
            PolarisCode(self.logs, binary=self.binary.with_name("missing"))
