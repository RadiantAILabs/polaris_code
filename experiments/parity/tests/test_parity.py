"""Tests for the parity tool.

They use a small stand-in agent (``stub_agent.py``) so that the whole tool
(endpoint, runner, extraction, comparison, report) runs without the real
binaries. Run them with::

    python3 -m unittest discover -s experiments/parity/tests -t experiments/parity
"""

import contextlib
import io
import json
import os
import stat
import sys
import tempfile
import unittest
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
sys.path.insert(0, str(ROOT))

from parity_tool import cli, extract, runner  # noqa: E402
from parity_tool.endpoint import AUXILIARY_TEXT, Endpoint  # noqa: E402
from parity_tool.session import SessionError, expand, load_session, load_sessions  # noqa: E402

STUB = HERE / "stub_agent.py"

RULES = "1. Do not change tests.\n2. Give up honestly.\n3. Report what ran."


def write_wrapper(folder: Path, flavour: str) -> Path:
    """An executable that runs the stub agent with ``flavour``."""
    path = folder / f"agent-{flavour}"
    path.write_text(
        f'#!/bin/sh\nexec "{sys.executable}" "{STUB}" --flavour={flavour} "$@"\n', encoding="utf-8"
    )
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    return path


def post(url: str, body: dict) -> dict:
    request = urllib.request.Request(
        url + "/openai/v1/responses",
        data=json.dumps(body).encode(),
        headers={"content-type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=10) as reply:
        return json.load(reply)


TOOLS = [{"type": "function", "name": "read_file", "parameters": {"type": "object"}}]


class EndpointTests(unittest.TestCase):
    def test_serves_replies_in_order_and_numbers_calls(self):
        script = [
            {"text": "one", "calls": [{"tool": "read_file", "arguments": {"path": "a"}}]},
            {"calls": [{"tool": "run_command", "arguments": {"command": "ls"}}, {"tool": "read_file"}]},
            {"text": "last"},
        ]
        with Endpoint(script) as endpoint:
            first = post(endpoint.url, {"input": "x", "tools": TOOLS})
            second = post(endpoint.url, {"input": "x", "tools": TOOLS})
            third = post(endpoint.url, {"input": "x", "tools": TOOLS})
        self.assertEqual([o["type"] for o in first["output"]], ["message", "function_call"])
        self.assertEqual(first["output"][1]["call_id"], "call_1")
        self.assertEqual([o["call_id"] for o in second["output"]], ["call_2", "call_3"])
        self.assertEqual(json.loads(second["output"][0]["arguments"]), {"command": "ls"})
        self.assertEqual(third["output"][0]["content"][0]["text"], "last")
        self.assertEqual(endpoint.beyond_script, 0)
        self.assertEqual(len(endpoint.requests), 3)

    def test_request_without_tools_does_not_use_a_reply(self):
        with Endpoint([{"text": "scripted"}]) as endpoint:
            aside = post(endpoint.url, {"input": "summarise"})
            main = post(endpoint.url, {"input": "go", "tools": TOOLS})
        self.assertEqual(aside["output"][0]["content"][0]["text"], AUXILIARY_TEXT)
        self.assertEqual(main["output"][0]["content"][0]["text"], "scripted")
        self.assertEqual([r["auxiliary"] for r in endpoint.requests], [True, False])

    def test_running_out_of_replies_is_counted(self):
        with Endpoint([{"text": "only"}]) as endpoint:
            post(endpoint.url, {"input": "a", "tools": TOOLS})
            post(endpoint.url, {"input": "b", "tools": TOOLS})
            post(endpoint.url, {"input": "c", "tools": TOOLS})
        self.assertEqual(endpoint.beyond_script, 2)

    def test_other_paths_are_marked_unexpected(self):
        with Endpoint([{"text": "x"}]) as endpoint:
            request = urllib.request.Request(endpoint.url + "/model/x/converse", data=b"{}")
            with self.assertRaises(urllib.error.HTTPError):
                urllib.request.urlopen(request, timeout=10)
        self.assertTrue(endpoint.requests[0]["unexpected"])


class SessionTests(unittest.TestCase):
    def test_expand_replaces_in_nested_values(self):
        value = {"a": ["{workspace}/x", 3], "b": "{session}"}
        self.assertEqual(
            expand(value, {"workspace": "/w", "session": "/s"}), {"a": ["/w/x", 3], "b": "/s"}
        )

    def test_bad_sessions_are_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "s").mkdir()
            (root / "s" / "workspace").mkdir()
            (root / "s" / "session.json").write_text('{"replies": []}')
            with self.assertRaises(SessionError):
                load_session(root / "s")
            (root / "s" / "session.json").write_text(
                '{"replies": [{"text": "x"}], "runs": [{"name": "a"}, {"name": "a"}]}'
            )
            with self.assertRaises(SessionError):
                load_session(root / "s")
            (root / "s" / "session.json").write_text(
                '{"replies": [{"text": "x"}], "workspace": "../x"}'
            )
            with self.assertRaises(SessionError):
                load_session(root / "s")

    def test_shared_workspace_is_found(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "_workspaces" / "base").mkdir(parents=True)
            (root / "s").mkdir()
            (root / "s" / "session.json").write_text(
                '{"replies": [{"text": "x"}], "workspace": "base"}'
            )
            session = load_session(root / "s")
            self.assertEqual(session.workspace, root / "_workspaces" / "base")
            self.assertEqual([s.name for s in load_sessions(root)], ["s"])

    def test_runs_add_their_arguments_to_the_shared_ones(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "s" / "workspace").mkdir(parents=True)
            (root / "s" / "session.json").write_text(
                json.dumps(
                    {
                        "replies": [{"text": "x"}],
                        "args": ["--rules-arm", "steps"],
                        "runs": [
                            {"name": "plain"},
                            {"name": "no-give-up", "args": ["--no-give-up"]},
                        ],
                    }
                )
            )
            specs = load_session(root / "s").run_specs()
        self.assertEqual([s.id for s in specs], ["s/plain", "s/no-give-up"])
        self.assertEqual(specs[1].args, ["--rules-arm", "steps", "--no-give-up"])


class ExtractTests(unittest.TestCase):
    def segment(self, text, item="message", role="user", call_id=""):
        return extract.Segment("input[1]", item, role, call_id, "", text)

    def test_prompt_section_is_cut_out_of_the_system_prompt(self):
        text = f"You are an agent.\n\n## Rules for this task\n\n{RULES}\n\n## Other\n\nmore"
        segment = extract.Segment("instructions", "instructions", "system", "", "", text)
        rest, items = extract.split_rule_text(segment)
        self.assertEqual([i.kind for i in items], ["prompt"])
        self.assertTrue(items[0].text.endswith("3. Report what ran."))
        self.assertIn("## Other", rest)
        self.assertNotIn("Do not change tests", rest)

    def test_refusal_is_a_whole_tool_result(self):
        segment = self.segment("Refused before it ran: no. Nothing was run.", "result", "tool", "call_1")
        rest, items = extract.split_rule_text(segment)
        self.assertEqual((rest, [i.kind for i in items]), ("", ["refusal"]))
        self.assertEqual(items[0].call_id, "call_1")

    def test_note_after_a_tool_result_keeps_the_stock_result(self):
        segment = self.segment("exit 0\n\n[Rules check] Protected files were changed.", "result", "tool", "call_2")
        rest, items = extract.split_rule_text(segment)
        self.assertEqual(rest.strip(), "exit 0")
        self.assertEqual([i.kind for i in items], ["note"])

    def test_hold_is_told_from_a_note(self):
        segment = self.segment("[Rules check] Your final message was not accepted. Fix it.")
        _, items = extract.split_rule_text(segment)
        self.assertEqual([i.kind for i in items], ["hold"])

    def test_model_text_is_never_read_for_rules(self):
        segment = self.segment("CANNOT_COMPLETE: no way", "message", "assistant")
        rest, items = extract.split_rule_text(segment)
        self.assertEqual(items, [])
        self.assertEqual(rest, "CANNOT_COMPLETE: no way")

    def test_timeline_lists_an_item_once_at_its_first_request(self):
        def view(number, texts):
            items = [extract.RuleItem("hold", "input[1]", "", t) for t in texts]
            return extract.RequestView(number, False, items, [], [], {}, {})

        views = [view(1, []), view(2, ["a"]), view(3, ["a", "b"]), view(4, ["a", "b", "a"])]
        found = [(a.request, a.text) for a in extract.timeline(views)]
        self.assertEqual(found, [(2, "a"), (3, "b"), (4, "a")])

    def test_record_fields_leave_out_times_and_normalise_paths(self):
        record = {
            "started_unix_ms": 5,
            "model_calls": [{"at_ms": 1, "n": 1}, {"at_ms": 2, "n": 2}],
            "checks": [{"duration_ms": 9, "output_tail": "ran in /tmp/x/ws"}],
            "inputs": {"workspace": "/tmp/x/ws"},
        }
        fields = extract.record_fields(record, [("/tmp/x/ws", "<workspace>")], extract.IGNORED_RECORD_FIELDS)
        self.assertEqual(
            fields,
            {
                "model_calls[0].n": 1,
                "model_calls[1].n": 2,
                "checks[0].output_tail": "ran in <workspace>",
                "inputs.workspace": "<workspace>",
            },
        )


class RunnerTests(unittest.TestCase):
    def test_environment_holds_only_placeholders(self):
        env = runner.sandbox_env(Path("/h"), Path("/t"), "http://127.0.0.1:1")
        self.assertEqual(env["AWS_EC2_METADATA_DISABLED"], "true")
        self.assertNotIn("AWS_PROFILE", env)
        self.assertTrue(env["AWS_ACCESS_KEY_ID"].startswith("placeholder"))
        self.assertEqual(env["AWS_ENDPOINT_URL"], "http://127.0.0.1:1")

    def test_environment_ignores_the_callers_credentials(self):
        saved = {k: os.environ.get(k) for k in ("AWS_PROFILE", "AWS_SESSION_TOKEN", "AWS_ACCESS_KEY_ID")}
        os.environ.update(AWS_PROFILE="real", AWS_SESSION_TOKEN="real", AWS_ACCESS_KEY_ID="real")
        try:
            env = runner.sandbox_env(Path("/h"), Path("/t"))
        finally:
            for key, value in saved.items():
                if value is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = value
        self.assertNotIn("AWS_PROFILE", env)
        self.assertNotIn("AWS_SESSION_TOKEN", env)
        self.assertNotEqual(env["AWS_ACCESS_KEY_ID"], "real")

    def test_rules_options_are_split_out(self):
        kept, dropped = runner.split_rules_options(
            ["--rules-arm", "steps", "--no-give-up", "--max-holds=2", "--session", "s", "--protect", "t.py"]
        )
        self.assertEqual(kept, ["--session", "s"])
        self.assertEqual(dropped, ["--rules-arm", "steps", "--no-give-up", "--max-holds=2", "--protect", "t.py"])

    def test_tree_digests_skip_hidden_and_bytecode(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "a.py").write_text("a")
            (root / "sub").mkdir()
            (root / "sub" / "b.py").write_text("b")
            (root / ".hidden").mkdir()
            (root / ".hidden" / "c").write_text("c")
            (root / "__pycache__").mkdir()
            (root / "__pycache__" / "a.pyc").write_text("x")
            (root / "scratch.txt").write_text("s")
            self.assertEqual(sorted(runner.tree_digests(root, [])), ["a.py", "scratch.txt", "sub/b.py"])
            self.assertEqual(sorted(runner.tree_digests(root, ["scratch.*"])), ["a.py", "sub/b.py"])


def make_session(root: Path) -> None:
    """A session that refuses a call, holds a final message once, then accepts."""
    folder = root / "gate"
    (folder / "workspace").mkdir(parents=True)
    (folder / "workspace" / "func.py").write_text("def add(a, b):\n    return 0\n")
    (folder / "workspace" / "test_func.py").write_text("from func import add\nassert add(1, 2) == 3\n")
    session = {
        "description": "A refused edit, one hold, then an accepted message.",
        "exercises": ["refusal", "hold"],
        "args": [
            "--rules-arm",
            "steps",
            "--protect",
            "test_func.py",
            "--test-command",
            "test -f fixed.txt",
            "--max-holds",
            "2",
        ],
        "replies": [
            {"calls": [{"tool": "edit_file", "arguments": {"path": "test_func.py"}}]},
            {"text": "I am done."},
            {"calls": [{"tool": "run_command", "arguments": {"command": "touch fixed.txt"}}]},
            {"text": "Now it is done."},
        ],
    }
    (folder / "session.json").write_text(json.dumps(session))


class EndToEndTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        base = Path(cls.tmp.name)
        cls.bin = base / "bin"
        cls.bin.mkdir()
        cls.agents = {
            flavour: write_wrapper(cls.bin, flavour)
            for flavour in ("base", "other-hold", "extra-call", "stock", "other-record")
        }
        cls.sessions = base / "sessions"
        cls.sessions.mkdir()
        make_session(cls.sessions)

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def run_tool(self, a: str, b: str):
        out = Path(self.tmp.name) / f"out-{a}-{b}"
        argv = [
            str(self.agents[a]),
            str(self.agents[b]),
            "--sessions",
            str(self.sessions),
            "--out",
            str(out),
            "--timeout",
            "60",
        ]
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            code = cli.main(argv)
        report = json.loads((out / "report.json").read_text())
        return code, report, (out / "report.txt").read_text()

    def test_same_build_twice_agrees(self):
        code, report, text = self.run_tool("base", "base")
        self.assertEqual(code, 0, text)
        run = report["runs"][0]
        self.assertEqual(run["verdict"], "agree")
        self.assertTrue(all(s["agrees"] for s in run["sections"].values()))
        self.assertIn("prompt@1, refusal@2, hold@3", " ".join(run["sections"]["rule text"]["lines"]))
        decisions = run["facts"]["decisions"]["old"]
        self.assertEqual(decisions["end reason"], "done")
        self.assertEqual(len(decisions["refusals the model was sent"]), 1)
        self.assertEqual(len(decisions["holds the model was sent"]), 1)

    def test_a_changed_hold_message_is_reported_with_its_request(self):
        code, report, text = self.run_tool("base", "other-hold")
        self.assertEqual(code, 1, text)
        run = report["runs"][0]
        self.assertEqual(run["verdict"], "differ")
        self.assertFalse(run["sections"]["rule text"]["agrees"])
        joined = "\n".join(run["sections"]["rule text"]["lines"])
        self.assertIn("request 3", joined)
        self.assertIn("was not accepted", joined)
        self.assertIn("is refused for now", joined)

    def test_an_extra_model_call_is_stated_plainly(self):
        code, report, text = self.run_tool("base", "extra-call")
        self.assertEqual(code, 1, text)
        run = report["runs"][0]
        lines = "\n".join(run["sections"]["model calls"]["lines"])
        self.assertIn("new also made a request outside the agent loop", lines)
        self.assertNotIn("old also made", lines)
        # The extra call must not shift the scripted replies of the loop.
        self.assertTrue(run["sections"]["rule text"]["agrees"])
        self.assertTrue(run["sections"]["decisions"]["agrees"])

    def test_a_build_without_rules_options_is_run_without_them(self):
        code, report, text = self.run_tool("base", "stock")
        self.assertEqual(code, 1, text)
        run = report["runs"][0]
        self.assertTrue(any("no rules options" in note for note in run["notes"]))
        self.assertFalse(run["sections"]["rule text"]["agrees"])
        self.assertIn("only old sent", "\n".join(run["sections"]["rule text"]["lines"]))

    def test_record_differences_are_found_but_times_are_not(self):
        code, report, text = self.run_tool("base", "other-record")
        self.assertEqual(code, 1, text)
        run = report["runs"][0]
        lines = "\n".join(run["sections"]["record"]["lines"])
        self.assertIn("extra_field", lines)
        self.assertNotIn("unix_ms", lines)
        self.assertTrue(run["sections"]["rule text"]["agrees"])
        self.assertTrue(run["sections"]["decisions"]["agrees"])

    def test_a_missing_binary_is_an_error(self):
        code = cli.main([str(self.bin / "missing"), str(self.agents["base"]), "--sessions", str(self.sessions)])
        self.assertEqual(code, 2)


class ShippedSessionTests(unittest.TestCase):
    """The sessions that ship with the tool."""

    REQUIRED = {
        "prompt",
        "refusal",
        "tamper",
        "gate",
        "give-up",
        "no-give-up",
        "rules-file",
        "max-holds",
        "max-messages",
    }

    def test_every_shipped_session_loads_and_the_required_mechanisms_are_covered(self):
        sessions = load_sessions(ROOT / "sessions")
        self.assertTrue(sessions)
        covered = {name for session in sessions for name in session.exercises}
        self.assertEqual(self.REQUIRED - covered, set())
        for session in sessions:
            self.assertTrue(session.description, session.name)
            self.assertTrue(session.run_specs())


if __name__ == "__main__":
    unittest.main()
