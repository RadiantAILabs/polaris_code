"""The installed-agent boundary is tested without a container or provider key."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

try:
    from harbor.models.agent.context import AgentContext

    from ..polaris_code import REMOTE_BINARY, PolarisCode

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
        return PolarisCode(
            self.logs, binary=self.binary, model_name="anthropic/test", **kwargs
        )

    async def test_setup_uploads_only_the_selected_binary_then_grants_execute_mode(
        self,
    ) -> None:
        environment = RecordingEnvironment()
        agent = self._agent()

        await agent.setup(environment)  # type: ignore[arg-type]

        self.assertEqual(environment.uploads, [(str(self.binary), REMOTE_BINARY)])
        commands = [str(call["command"]) for call in environment.execs]
        self.assertTrue(
            any(
                command.endswith("chmod 755 /installed-agent/polaris-code")
                for command in commands
            )
        )
        self.assertNotIn("ANTHROPIC_API_KEY", "\n".join(commands))

    async def test_turn_has_the_original_instruction_model_and_only_explicit_shell_grants(
        self,
    ) -> None:
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

    def test_release_requires_an_explicit_pin_and_excludes_a_local_binary(self) -> None:
        for kwargs in (
            {},
            {"version": "latest"},
            {"version": "0.3.0; echo bad"},
            {"version": "0.3.0", "binary": self.binary},
        ):
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                PolarisCode(self.logs, **kwargs)
        self.assertEqual(PolarisCode(self.logs, version="v0.3.0").version(), "0.3.0")
        self.assertTrue(self._agent().version().startswith("sha256:"))

    async def test_release_setup_downloads_checksums_for_both_linux_architectures(
        self,
    ) -> None:
        from unittest.mock import AsyncMock

        environment = RecordingEnvironment()
        agent = PolarisCode(self.logs, version="v0.3.0")
        agent.ensure_system_dependencies = AsyncMock()
        await agent.setup(environment)  # type: ignore[arg-type]
        self.assertEqual(environment.uploads, [])
        agent.ensure_system_dependencies.assert_awaited_once_with(
            environment, ("curl", "tar")
        )
        install = str(environment.execs[-1]["command"])
        self.assertIn("x86_64-unknown-linux-gnu", install)
        self.assertIn("aarch64-unknown-linux-gnu", install)
        self.assertIn("releases/download/v0.3.0/", install)
        self.assertIn('sha256sum -c "$archive.sha256"', install)
        self.assertLess(install.index("sha256sum -c"), install.index("tar -xzf"))
        self.assertIn("--version", install)

    async def test_board_options_and_trajectory_path_reach_the_executable(self) -> None:
        import shlex
        from pathlib import PurePosixPath

        environment = RecordingEnvironment()
        agent = self._agent(
            provider="openai-chat",
            base_url="http://127.0.0.1:8000/v1",
            reasoning_effort="high",
            context_window="262144",
            max_output_tokens=32768,
            unattended="true",
            environment_logs_dir=PurePosixPath("/logs/custom"),
            extra_env={"OPENAI_API_KEY": "test-key"},
        )
        instruction = "Fix it; quotes ' and $(touch /tmp/bad) stay data."
        await agent.run(instruction, environment, AgentContext())  # type: ignore[arg-type]
        call = environment.execs[-1]
        argv = shlex.split(str(call["command"]))
        self.assertEqual(argv[argv.index("-p") + 1], instruction)
        for flag, expected in (
            ("--provider", "openai-chat"),
            ("--reasoning-effort", "high"),
            ("--context-window", "262144"),
            ("--max-output-tokens", "32768"),
            ("--trajectory-path", "/logs/custom/trajectory.json"),
        ):
            self.assertEqual(argv[argv.index(flag) + 1], expected)
        self.assertIn("--dangerously-skip-permissions", argv)
        self.assertNotIn("--allow-edit", argv)
        self.assertNotIn("test-key", str(call))
        self.assertIsNone(call.get("cwd"))
        self.assertEqual(call["env"]["OPENAI_BASE_URL"], "http://127.0.0.1:8000/v1")

    async def test_false_string_does_not_grant_unattended_execution(self) -> None:
        environment = RecordingEnvironment()
        await self._agent(unattended="false").run("x", environment, AgentContext())  # type: ignore[arg-type]
        self.assertNotIn(
            "--dangerously-skip-permissions", str(environment.execs[-1]["command"])
        )

    def test_model_limits_are_positive_integers(self) -> None:
        for name in ("context_window", "max_output_tokens"):
            for value in (0, -1, True, 1.5, "not-a-number"):
                with (
                    self.subTest(name=name, value=value),
                    self.assertRaisesRegex(ValueError, name),
                ):
                    self._agent(**{name: value})

    def test_native_atif_populates_tokens_cache_and_known_cost(self) -> None:
        import json

        self.logs.mkdir()
        trajectory = {
            "schema_version": "ATIF-v1.6",
            "session_id": "test-session",
            "agent": {"name": "polaris-code", "version": "test"},
            "steps": [{"step_id": 1, "source": "user", "message": "Fix it"}],
            "final_metrics": {
                "total_prompt_tokens": 100,
                "total_completion_tokens": 20,
                "total_cached_tokens": 40,
                "total_cost_usd": 0.125,
            },
        }
        path = self.logs / "trajectory.json"
        path.write_text(json.dumps(trajectory))
        context = AgentContext()
        self._agent().populate_context_post_run(context)
        self.assertEqual(
            (
                context.n_input_tokens,
                context.n_output_tokens,
                context.n_cache_tokens,
                context.cost_usd,
            ),
            (100, 20, 40, 0.125),
        )
        self.assertEqual(json.loads(path.read_text()), trajectory)
        del trajectory["final_metrics"]["total_cost_usd"]
        path.write_text(json.dumps(trajectory))
        context = AgentContext()
        self._agent().populate_context_post_run(context)
        self.assertIsNone(context.cost_usd)

    def test_absent_or_invalid_trajectory_preserves_context_and_logs_a_warning(
        self,
    ) -> None:
        self.logs.mkdir()
        for content in (None, "{broken", '{"schema_version":"ATIF-v1.6"}'):
            if content is not None:
                (self.logs / "trajectory.json").write_text(content)
            context = AgentContext(n_input_tokens=7)
            agent = self._agent()
            with self.assertLogs(agent.logger, level="WARNING"):
                agent.populate_context_post_run(context)
            self.assertEqual(context.n_input_tokens, 7)
            self.assertIsNone(context.cost_usd)

    def test_release_script_installs_each_cpu_and_refuses_bad_checksums(self) -> None:
        import hashlib
        import io
        import os
        import subprocess
        import tarfile

        from ..polaris_code import release_install_command

        root = Path(self.directory.name)
        tools = root / "tools"
        tools.mkdir()
        (tools / "uname").write_text(
            '#!/bin/sh\ncase "$1" in -s) echo Linux ;; -m) echo "$TEST_ARCH" ;; esac\n'
        )
        (tools / "curl").write_text(
            '#!/bin/sh\nwhile [ "$1" != -o ]; do url="$1"; shift; done\ncp "$TEST_RELEASES/${url##*/}" "$2"\n'
        )
        for tool in tools.iterdir():
            tool.chmod(0o755)
        for arch, target, valid in (
            ("x86_64", "x86_64", True),
            ("aarch64", "aarch64", True),
            ("x86_64", "x86_64", False),
        ):
            with self.subTest(arch=arch, checksum=valid):
                archive = root / f"polaris-code-0.3.0-{target}-unknown-linux-gnu.tar.gz"
                binary = b"#!/bin/sh\necho 'polaris 0.3.0'\n"
                with tarfile.open(archive, "w:gz") as tar:
                    info = tarfile.TarInfo("polaris")
                    info.size = len(binary)
                    info.mode = 0o755
                    tar.addfile(info, io.BytesIO(binary))
                digest = (
                    hashlib.sha256(archive.read_bytes()).hexdigest()
                    if valid
                    else "0" * 64
                )
                archive.with_name(archive.name + ".sha256").write_text(
                    f"{digest}  {archive.name}\n"
                )
                installed = root / "installed-polaris"
                installed.unlink(missing_ok=True)
                command = release_install_command("0.3.0").replace(
                    REMOTE_BINARY, str(installed)
                )
                result = subprocess.run(
                    ["bash", "-c", command],
                    check=False,
                    capture_output=True,
                    text=True,
                    env={
                        **os.environ,
                        "PATH": f"{tools}:{os.environ['PATH']}",
                        "TEST_ARCH": arch,
                        "TEST_RELEASES": str(root),
                    },
                )
                self.assertEqual(result.returncode == 0, valid, result.stderr)
                self.assertEqual(installed.exists(), valid)
                if valid:
                    self.assertEqual(installed.read_bytes(), binary)
