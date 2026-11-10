"""The installed-agent boundary is tested without a container or provider key."""

from __future__ import annotations

import asyncio
import hashlib
import importlib.util
import io
import json
import os
import shlex
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path, PurePosixPath
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

# Only a missing harbor skips these tests. CI sets POLARIS_REQUIRE_HARBOR so that
# a skip there is an error.
HARBOR_INSTALLED = importlib.util.find_spec("harbor") is not None
if not HARBOR_INSTALLED and os.environ.get("POLARIS_REQUIRE_HARBOR"):
    raise ModuleNotFoundError("harbor is required: POLARIS_REQUIRE_HARBOR is set")
if HARBOR_INSTALLED:
    # With harbor present, an import error here is a real failure: do not skip.
    from harbor.agents.installed.base import NonZeroAgentExitCodeError
    from harbor.models.agent.context import AgentContext

    from .. import polaris_code as adapter
    from ..polaris_code import REMOTE_BINARY, PolarisCode, release_install_command

USAGE_SCHEMA = "polaris-code-run-usage/v1"
TOKENS = {"total_prompt_tokens": 100, "total_completion_tokens": 20}


def usage_line(
    cost: object,
    *,
    io_type: object = "usage",
    schema: str = USAGE_SCHEMA,
    source: object = "System",
) -> str:
    """One stream-json line carrying a usage record, as the application writes it."""
    usage = {"schema": schema, "estimated_cost_usd": cost}
    return json.dumps(
        {
            "source": source,
            "metadata": {"io_type": io_type},
            "content": {"Text": json.dumps(usage)},
        }
    )


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


class LocalShellEnvironment(RecordingEnvironment):
    """Runs each command in a local bash, with a stub in place of the executable."""

    def __init__(self, stub: Path, cwd: str, stdin: str) -> None:
        super().__init__()
        self.stub, self.cwd, self.stdin = stub, cwd, stdin

    async def exec(self, **kwargs: object) -> SimpleNamespace:
        self.execs.append(kwargs)
        command = str(kwargs["command"]).replace(
            REMOTE_BINARY, shlex.quote(str(self.stub))
        )
        env = {**os.environ, **dict(kwargs.get("env") or {})}  # type: ignore[call-overload]
        process = await asyncio.create_subprocess_exec(
            "bash",
            "-c",
            command,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            cwd=self.cwd,
            env=env,
        )
        stdout, stderr = await process.communicate(self.stdin.encode())
        return SimpleNamespace(
            return_code=process.returncode,
            stdout=stdout.decode(),
            stderr=stderr.decode(),
        )


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
            {"version": "\u0661.\u0662.\u0663"},  # Arabic-Indic digits
            {"version": "0.3.0", "binary": self.binary},
        ):
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                PolarisCode(self.logs, **kwargs)
        with self.assertRaisesRegex(ValueError, "'latest'"):
            release_install_command("latest")
        self.assertEqual(PolarisCode(self.logs, version="v0.3.0").version(), "0.3.0")
        self.assertTrue(self._agent().version().startswith("sha256:"))

    def test_options_are_keyword_only(self) -> None:
        # A second positional argument is Harbor's, never `binary` or `version`.
        with self.assertRaisesRegex(ValueError, "exactly one"):
            PolarisCode(self.logs, self.binary)

    def test_a_release_digest_pin_needs_a_release_and_a_sha256_digest(self) -> None:
        digest = "ab" * 32
        PolarisCode(self.logs, version="0.3.0", sha256=digest)
        for kwargs in (
            {"version": "0.3.0", "sha256": "abc"},
            {"version": "0.3.0", "sha256": "g" * 64},
            {"version": "0.3.0", "sha256": digest + "\n"},
            {"binary": self.binary, "sha256": digest},
        ):
            with (
                self.subTest(kwargs=kwargs),
                self.assertRaisesRegex(ValueError, "sha256"),
            ):
                PolarisCode(self.logs, **kwargs)

    def test_unattended_cannot_be_combined_with_command_patterns(self) -> None:
        for unattended in (True, "true", "1"):
            with (
                self.subTest(unattended=unattended),
                self.assertRaisesRegex(ValueError, "allow_command"),
            ):
                self._agent(unattended=unattended, allow_command=["pytest *"])
        # These are not the rejected pair.
        self._agent(unattended=True, allow_command=[])
        self._agent(unattended="false", allow_command="pytest *")

    async def test_a_command_string_is_one_pattern_and_graph_search_is_checked(
        self,
    ) -> None:
        environment = RecordingEnvironment()
        agent = self._agent(allow_command="pytest *")
        await agent.run("x", environment, AgentContext())  # type: ignore[arg-type]
        argv = shlex.split(str(environment.execs[-1]["command"]))
        self.assertEqual(argv.count("--allow-command"), 1)
        self.assertEqual(argv[argv.index("--allow-command") + 1], "pytest *")
        with self.assertRaisesRegex(ValueError, "graph_search"):
            self._agent(graph_search="bogus")

    async def test_an_instruction_that_starts_with_a_dash_stays_a_value(self) -> None:
        environment = RecordingEnvironment()
        instruction = "--help me; -x"
        await self._agent().run(instruction, environment, AgentContext())  # type: ignore[arg-type]
        argv = shlex.split(str(environment.execs[-1]["command"]))
        self.assertIn(f"--print={instruction}", argv)
        self.assertNotIn("-p", argv)
        self.assertNotIn(instruction, argv)

    async def test_release_setup_downloads_checksums_for_linux_x86_64(
        self,
    ) -> None:
        environment = RecordingEnvironment()
        agent = PolarisCode(self.logs, version="v0.3.0")
        agent.ensure_system_dependencies = AsyncMock()
        await agent.setup(environment)  # type: ignore[arg-type]
        self.assertEqual(environment.uploads, [])
        agent.ensure_system_dependencies.assert_awaited_once_with(
            environment, ("curl", "tar")
        )
        install = str(environment.execs[-1]["command"])
        self.assertIn("x86_64-unknown-linux-musl", install)
        self.assertNotIn("x86_64-unknown-linux-gnu", install)
        self.assertIn("supports x86_64-unknown-linux-musl only", install)
        self.assertIn("releases/download/v0.3.0/", install)
        self.assertIn("--proto '=https'", install)
        self.assertIn('"$archive.sha256"', install)
        self.assertIn("sha256sum -c -", install)
        self.assertLess(install.index("sha256sum -c"), install.index("tar -xzf"))
        self.assertIn("--version", install)

    async def test_release_setup_with_a_digest_pin_checks_that_digest(self) -> None:
        environment = RecordingEnvironment()
        agent = PolarisCode(self.logs, version="v0.3.0", sha256="AB" * 32)
        agent.ensure_system_dependencies = AsyncMock()
        await agent.setup(environment)  # type: ignore[arg-type]
        install = str(environment.execs[-1]["command"])
        self.assertIn(f'echo "{"ab" * 32}  $archive" | sha256sum -c -', install)
        self.assertNotIn(".sha256", install)

    async def test_board_options_and_trajectory_path_reach_the_executable(self) -> None:
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
        self.assertIn(f"--print={instruction}", argv)
        self.assertNotIn(instruction, argv)
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

    async def test_image_input_reaches_the_executable_only_when_set(self) -> None:
        for value in ("on", "off", "auto"):
            with self.subTest(image_input=value):
                environment = RecordingEnvironment()
                await self._agent(image_input=value).run(
                    "x",
                    environment,
                    AgentContext(),  # type: ignore[arg-type]
                )
                argv = shlex.split(str(environment.execs[-1]["command"]))
                self.assertEqual(argv[argv.index("--image-input") + 1], value)
        environment = RecordingEnvironment()
        await self._agent().run("x", environment, AgentContext())  # type: ignore[arg-type]
        self.assertNotIn("--image-input", str(environment.execs[-1]["command"]))

    def test_image_input_must_be_auto_on_or_off(self) -> None:
        for value in ("maybe", "", "ON", True, 1):
            with (
                self.subTest(value=value),
                self.assertRaisesRegex(ValueError, "image_input"),
            ):
                self._agent(image_input=value)

    def test_model_limits_are_positive_integers(self) -> None:
        for name in ("context_window", "max_output_tokens"):
            for value in (0, -1, True, 1.5, "not-a-number", 2**32):
                with (
                    self.subTest(name=name, value=value),
                    self.assertRaisesRegex(ValueError, name),
                ):
                    self._agent(**{name: value})
        self._agent(context_window=2**32 - 1)
        self._agent(max_output_tokens=2**32 - 1)

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

    def _release_archive(self) -> tuple[Path, bytes]:
        """Write a release archive holding a stub `polaris`; return it and its bytes."""
        archive = (
            Path(self.directory.name)
            / "polaris-code-0.3.0-x86_64-unknown-linux-musl.tar.gz"
        )
        binary = b"#!/bin/sh\necho 'polaris 0.3.0'\n"
        with tarfile.open(archive, "w:gz") as tar:
            info = tarfile.TarInfo("polaris")
            info.size = len(binary)
            info.mode = 0o755
            tar.addfile(info, io.BytesIO(binary))
        return archive, binary

    def _run_release_script(
        self, command: str, *, arch: str = "x86_64", os_name: str = "Linux"
    ) -> tuple[subprocess.CompletedProcess[str], Path]:
        """Run an install script with stub `uname` and `curl` and a local release."""
        root = Path(self.directory.name)
        tools = root / "tools"
        tools.mkdir(exist_ok=True)
        (tools / "uname").write_text(
            '#!/bin/sh\ncase "$1" in -s) echo "$TEST_OS" ;; -m) echo "$TEST_ARCH" ;; esac\n'
        )
        (tools / "curl").write_text(
            '#!/bin/sh\nwhile [ "$1" != -o ]; do url="$1"; shift; done\ncp "$TEST_RELEASES/${url##*/}" "$2"\n'
        )
        for tool in tools.iterdir():
            tool.chmod(0o755)
        installed = root / "installed-polaris"
        installed.unlink(missing_ok=True)
        result = subprocess.run(
            ["bash", "-c", command.replace(REMOTE_BINARY, str(installed))],
            check=False,
            capture_output=True,
            text=True,
            env={
                **os.environ,
                "PATH": f"{tools}:{os.environ['PATH']}",
                "TEST_ARCH": arch,
                "TEST_OS": os_name,
                "TEST_RELEASES": str(root),
            },
        )
        return result, installed

    def test_release_script_installs_x86_64_and_refuses_other_systems_or_bad_checksums(
        self,
    ) -> None:
        archive, binary = self._release_archive()
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        listing = archive.with_name(archive.name + ".sha256")
        command = release_install_command("0.3.0")
        good = f"{digest}  {archive.name}"
        for arch, os_name, line, installs, message in (
            ("x86_64", "Linux", good, True, ""),
            ("amd64", "Linux", good, True, ""),
            ("x86_64", "Linux", f"{digest} *{archive.name}", True, ""),
            (
                "aarch64",
                "Linux",
                good,
                False,
                "supports x86_64-unknown-linux-musl only",
            ),
            ("x86_64", "Darwin", good, False, "requires Linux"),
            ("x86_64", "Linux", f"{'0' * 64}  {archive.name}", False, "FAILED"),
            # A valid digest of another file must not vouch for the archive.
            ("x86_64", "Linux", f"{digest}  /bin/true", False, "does not name"),
            ("x86_64", "Linux", "", False, "does not name"),
        ):
            with self.subTest(arch=arch, os_name=os_name, line=line):
                listing.write_text(f"{line}\n" if line else "")
                result, installed = self._run_release_script(
                    command, arch=arch, os_name=os_name
                )
                self.assertEqual(result.returncode == 0, installs, result.stderr)
                self.assertEqual(installed.exists(), installs)
                self.assertIn(message, result.stdout + result.stderr)
                if installs:
                    self.assertEqual(installed.read_bytes(), binary)

    def test_release_script_with_a_digest_pin_ignores_the_release_checksum_file(
        self,
    ) -> None:
        archive, binary = self._release_archive()
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        listing = archive.with_name(archive.name + ".sha256")
        listing.unlink(missing_ok=True)  # A pinned script must not fetch it.
        for pin, installs in (
            (digest, True),
            (digest.upper(), True),
            ("0" * 64, False),
        ):
            with self.subTest(pin=pin):
                result, installed = self._run_release_script(
                    release_install_command("0.3.0", pin)
                )
                self.assertEqual(result.returncode == 0, installs, result.stderr)
                self.assertEqual(installed.exists(), installs)
                if installs:
                    self.assertEqual(installed.read_bytes(), binary)
                else:
                    self.assertIn("FAILED", result.stdout + result.stderr)
        # A matching release checksum file does not rescue a wrong pin.
        listing.write_text(f"{digest}  {archive.name}\n")
        result, installed = self._run_release_script(
            release_install_command("0.3.0", "0" * 64)
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(installed.exists())

    async def test_nonzero_process_exit_is_not_hidden_by_log_redirection(self) -> None:
        class FailedEnvironment(RecordingEnvironment):
            async def exec(self, **kwargs: object) -> SimpleNamespace:
                self.execs.append(kwargs)
                return SimpleNamespace(return_code=42, stdout="", stderr="")

        environment = FailedEnvironment()
        with self.assertRaises(NonZeroAgentExitCodeError):
            await self._agent().run("task", environment, AgentContext())  # type: ignore[arg-type]

    async def test_the_generated_command_logs_both_streams_and_keeps_the_exit_status(
        self,
    ) -> None:
        root = Path(self.directory.name)
        # The log directory has a space and a quote, so quoting is exercised.
        logs = root / "logs dir" / "it's"
        logs.mkdir(parents=True)
        record = root / "record"
        record.mkdir()
        instruction = "Fix it; quotes ' and $(touch pwned) stay data."
        for exit_code in (0, 42):
            with self.subTest(exit_code=exit_code):
                stub = root / "stub-polaris"
                stub.write_text(
                    "#!/bin/sh\n"
                    f'printf "%s\\n" "$@" > {shlex.quote(str(record / "argv"))}\n'
                    f"cat > {shlex.quote(str(record / 'stdin'))}\n"
                    f'echo "$POLARIS_CODE_GRAPH_SEARCH" > {shlex.quote(str(record / "graph"))}\n'
                    "echo stub-stdout\n"
                    "echo stub-stderr >&2\n"
                    f"exit {exit_code}\n"
                )
                stub.chmod(0o755)
                environment = LocalShellEnvironment(stub, str(root), "leaked stdin")
                agent = self._agent(
                    graph_search="curated", environment_logs_dir=PurePosixPath(logs)
                )
                if exit_code == 0:
                    await agent.run(instruction, environment, AgentContext())  # type: ignore[arg-type]
                else:
                    with self.assertRaisesRegex(
                        NonZeroAgentExitCodeError, "exit 42"
                    ) as raised:
                        await agent.run(instruction, environment, AgentContext())  # type: ignore[arg-type]
                    # Both streams went to the log files, not back to Harbor.
                    self.assertNotIn("stub-stdout", str(raised.exception))
                    self.assertNotIn("stub-stderr", str(raised.exception))
                self.assertEqual((logs / "polaris.jsonl").read_text(), "stub-stdout\n")
                self.assertEqual(
                    (logs / "polaris-stderr.txt").read_text(), "stub-stderr\n"
                )
                self.assertEqual((record / "stdin").read_bytes(), b"")
                self.assertEqual((record / "graph").read_text(), "curated\n")
                argv = (record / "argv").read_text().splitlines()
                self.assertIn(f"--print={instruction}", argv)
                self.assertEqual(
                    argv[argv.index("--trajectory-path") + 1],
                    f"{logs}/trajectory.json",
                )
                self.assertFalse((root / "pwned").exists())

    async def test_native_model_credentials_are_not_resolved_from_the_host(
        self,
    ) -> None:
        from unittest.mock import patch

        with patch.dict(
            "os.environ",
            {
                "HF_TOKEN": "worker-write-fixture",
                "HUGGINGFACE_API_KEY": "worker-write-fixture",
            },
        ):
            agent = PolarisCode(
                self.logs,
                binary=self.binary,
                model_name="openai-chat/org/model:provider",
                provider="openai-chat",
                extra_env={"OPENAI_API_KEY": "inference-fixture"},
            )
            self.assertEqual(agent.model_connection.env, {})
            environment = RecordingEnvironment()
            await agent.run("task", environment, AgentContext())  # type: ignore[arg-type]
        self.assertNotIn("worker-write-fixture", str(environment.execs))
        self.assertNotIn("inference-fixture", str(environment.execs))

    def test_context_window_leaves_room_for_input(self) -> None:
        for output in (99, 100, 101):
            with (
                self.subTest(output=output),
                self.assertRaisesRegex(ValueError, "context_window must exceed"),
            ):
                self._agent(context_window=100, max_output_tokens=output)
        self._agent(context_window=100, max_output_tokens=98)

    def test_known_cost_comes_from_a_system_usage_event_when_atif_has_no_price(
        self,
    ) -> None:
        import json

        self.logs.mkdir()
        trajectory = {
            "schema_version": "ATIF-v1.6",
            "agent": {"name": "polaris-code", "version": "test"},
            "steps": [{"step_id": 1, "source": "user", "message": "task"}],
            "final_metrics": {
                "total_prompt_tokens": 100,
                "total_completion_tokens": 20,
            },
        }
        (self.logs / "trajectory.json").write_text(json.dumps(trajectory))
        for value, expected in (
            (0.75, 0.75),
            (0, 0.0),
            (None, None),
            (-1, None),
            (True, None),
            ("0.75", None),
            (float("nan"), None),
        ):
            with self.subTest(value=value):
                event = {
                    "source": "System",
                    "metadata": {"io_type": "usage"},
                    "content": {
                        "Text": json.dumps(
                            {
                                "schema": "polaris-code-run-usage/v1",
                                "estimated_cost_usd": value,
                            }
                        )
                    },
                }
                path = self.logs / "polaris.jsonl"
                path.write_text(json.dumps(event) + "\nnot JSON\n")
                context = AgentContext()
                self._agent().populate_context_post_run(context)
                self.assertEqual(context.cost_usd, expected)
                self.assertEqual(context.n_input_tokens, 100)
                event["source"] = {"Agent": "code"}
                path.write_text(json.dumps(event) + "\n")
                context = AgentContext()
                self._agent().populate_context_post_run(context)
                self.assertIsNone(context.cost_usd)

    def _write_atif(self, final_metrics: dict[str, object] | None) -> None:
        self.logs.mkdir(exist_ok=True)
        trajectory = {
            "schema_version": "ATIF-v1.6",
            "agent": {"name": "polaris-code", "version": "test"},
            "steps": [{"step_id": 1, "source": "user", "message": "task"}],
            "final_metrics": final_metrics,
        }
        (self.logs / "trajectory.json").write_text(json.dumps(trajectory))

    def _cost_from_stream(self, *lines: str | bytes) -> float | None:
        """Cost the adapter reports for an unpriced ATIF and these stream lines."""
        self._write_atif(TOKENS)
        (self.logs / "polaris.jsonl").write_bytes(
            b"\n".join(
                line.encode() if isinstance(line, str) else line for line in lines
            )
        )
        context = AgentContext()
        self._agent().populate_context_post_run(context)
        self.assertEqual(context.n_input_tokens, 100)
        return context.cost_usd

    def test_only_a_system_usage_event_with_the_known_schema_sets_the_cost(
        self,
    ) -> None:
        text = json.dumps({"schema": USAGE_SCHEMA, "estimated_cost_usd": 0.5})
        for name, lines, expected in (
            ("usage event", [usage_line(0.5)], 0.5),
            ("other io_type", [usage_line(0.5, io_type="output")], None),
            ("unknown schema", [usage_line(0.5, schema=USAGE_SCHEMA + "0")], None),
            ("other source", [usage_line(0.5, source={"Agent": "code"})], None),
            (
                "no io_type",
                [json.dumps({"source": "System", "content": {"Text": text}})],
                None,
            ),
            (
                "no content",
                [json.dumps({"source": "System", "metadata": {"io_type": "usage"}})],
                None,
            ),
            (
                "text is not JSON",
                [
                    json.dumps(
                        {
                            "source": "System",
                            "metadata": {"io_type": "usage"},
                            "content": {"Text": "nope"},
                        }
                    )
                ],
                None,
            ),
            ("events are not objects", ["[]", '"usage"', "7", "null", ""], None),
            ("last valid event wins", [usage_line(0.5), usage_line(0.75)], 0.75),
            (
                "later invalid lines keep the earlier cost",
                [
                    usage_line(0.5),
                    usage_line(9, io_type="output"),
                    usage_line(-1),
                    "{",
                ],
                0.5,
            ),
        ):
            with self.subTest(name):
                self.assertEqual(self._cost_from_stream(*lines), expected)
        self._write_atif(TOKENS)
        (self.logs / "polaris.jsonl").unlink()
        context = AgentContext()
        self._agent().populate_context_post_run(context)
        self.assertIsNone(context.cost_usd)

    def test_undecodable_or_deeply_nested_log_lines_cannot_break_the_hook(
        self,
    ) -> None:
        deep = "[" * 200_000
        deep_text = json.dumps(
            {
                "source": "System",
                "metadata": {"io_type": "usage"},
                "content": {"Text": deep},
            }
        )
        good = usage_line(0.5)
        for name, lines in (
            ("invalid UTF-8 byte", [b"\xff\xfe", good]),
            ("truncated multibyte tail", [good, "\u00e9".encode()[:1]]),
            ("deep nesting", [deep, good]),
            ("deep nesting in the usage text", [deep_text, good]),
        ):
            with self.subTest(name):
                self.assertEqual(self._cost_from_stream(*lines), 0.5)

    def test_only_the_end_of_a_large_stream_log_is_read(self) -> None:
        padding = ["x" * 100] * 100
        with patch.object(adapter, "MAX_STREAM_BYTES", 4096):
            self.assertIsNone(self._cost_from_stream(usage_line(0.5), *padding))
            self.assertEqual(self._cost_from_stream(*padding, usage_line(0.75)), 0.75)

    def test_a_known_atif_cost_wins_over_the_stream_even_when_zero(self) -> None:
        for atif, expected in ((0.125, 0.125), (0, 0.0)):
            with self.subTest(atif=atif):
                self._write_atif({**TOKENS, "total_cost_usd": atif})
                (self.logs / "polaris.jsonl").write_text(usage_line(0.75) + "\n")
                context = AgentContext()
                self._agent().populate_context_post_run(context)
                self.assertIsNotNone(context.cost_usd)
                self.assertEqual(context.cost_usd, expected)

    def test_atif_without_final_metrics_uses_the_stream_cost_only(self) -> None:
        self._write_atif(None)
        (self.logs / "polaris.jsonl").write_text(usage_line(0.75) + "\n")
        context = AgentContext(n_input_tokens=7)
        self._agent().populate_context_post_run(context)
        self.assertEqual((context.n_input_tokens, context.cost_usd), (7, 0.75))
