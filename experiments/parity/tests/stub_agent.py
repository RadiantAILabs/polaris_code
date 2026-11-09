#!/usr/bin/env python3
"""A tiny stand-in for the agent binary, used only to test the parity tool.

It speaks just enough of the agent's interface: the same command line, one
``POST /openai/v1/responses`` per model call, three tools, a prompt section, a
refusal, a note and a hold, and a run record. Its behaviour is switched by the
``--flavour`` argument, which the test wrappers put first:

``base``           the reference behaviour.
``other-hold``     the hold message has different words.
``extra-call``     makes one request without tool definitions before the loop.
``stock``          has no rules options at all (``--help`` does not list them).
``other-record``   adds one field to the record and changes a time.
``reach-out``      asks for a web page outside the model endpoint, as a build that
                   fetches a price table would. The request must be refused.
"""

import json
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

RULES = "1. Do not change tests.\n2. Give up honestly.\n3. Report what ran."
SECTION = "## Rules for this task\n\n" + RULES


def post(url, body):
    request = urllib.request.Request(
        url + "/openai/v1/responses",
        data=json.dumps(body).encode(),
        headers={"content-type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=30) as reply:
        return json.load(reply)


def main():
    argv = sys.argv[1:]
    flavour = "base"
    if argv and argv[0].startswith("--flavour="):
        flavour = argv.pop(0).split("=", 1)[1]
    if "--help" in argv:
        print("usage: polaris [WORKSPACE] -p TASK" + ("" if flavour == "stock" else " --rules-arm ARM"))
        return 0
    workspace = Path(argv.pop(0))
    options = {}
    index = 0
    while index < len(argv):
        name = argv[index]
        if name in ("--no-give-up", "--allow-edit"):
            options[name] = True
        else:
            index += 1
            options[name] = argv[index]
        index += 1
    if flavour == "stock" and any(
        n in options for n in ("--rules-arm", "--protect", "--run-record")
    ):
        print("error: unexpected argument '--rules-arm'", file=sys.stderr)
        return 2

    import os

    url = os.environ["AWS_ENDPOINT_URL"]
    arm = options.get("--rules-arm", "none")
    protect = options.get("--protect")
    test_command = options.get("--test-command")
    holds_left = int(options.get("--max-holds", "3"))
    record_path = options.get("--run-record")
    instructions = f"You are a coding agent.\nWorking directory: {workspace}\n"
    if arm in ("prompt", "per-call", "steps", "prompt-gate"):
        instructions += "\n" + SECTION
    tools = [
        {"type": "function", "name": name, "parameters": {"type": "object"}}
        for name in ("read_file", "edit_file", "run_command")
    ]
    if flavour == "extra-call":
        post(url, {"model": "m", "instructions": "summarise", "input": "hello"})
    if flavour == "reach-out":
        try:
            urllib.request.urlopen("http://prices.example.test/prices.json?key=secret", timeout=10)
        except OSError:
            pass

    items = [{"type": "message", "role": "user", "content": options["-p"]}]
    record = {
        "arm": arm,
        "started_unix_ms": int(time.time() * 1000),
        "refusals": [],
        "gate_events": [],
        "tool_calls": [],
        "end_reason": "unfinished",
        "inputs": {"workspace": str(workspace)},
    }
    holds = 0
    final = ""
    for _ in range(int(options.get("--max-turns", "12"))):
        reply = post(
            url,
            {"model": "m", "instructions": instructions, "input": items, "tools": tools},
        )
        calls = [o for o in reply["output"] if o["type"] == "function_call"]
        items.extend(o for o in reply["output"] if o["type"] in ("function_call", "message"))
        if calls:
            for call in calls:
                args = json.loads(call["arguments"])
                refused = (
                    arm in ("per-call", "steps")
                    and protect
                    and call["name"] == "edit_file"
                    and args.get("path") == protect
                )
                if refused:
                    output = (
                        "Refused before it ran: the file is a protected test. Nothing was run.\n\n"
                        "This breaks rule 1 of the rules for this task:\n" + RULES
                    )
                    record["refusals"].append({"tool": "edit_file", "tool_call_id": call["call_id"]})
                elif call["name"] == "read_file":
                    output = (workspace / args["path"]).read_text()
                elif call["name"] == "run_command":
                    done = subprocess.run(
                        args["command"], shell=True, cwd=workspace, capture_output=True, text=True
                    )
                    output = done.stdout + done.stderr + f"exit {done.returncode}"
                else:
                    output = "ok"
                record["tool_calls"].append({"name": call["name"], "outcome": "refused" if refused else "ok"})
                items.append({"type": "function_call_output", "call_id": call["call_id"], "output": output})
            continue
        final = "".join(
            part["text"]
            for o in reply["output"]
            if o["type"] == "message"
            for part in o["content"]
        )
        if arm in ("steps", "prompt-gate") and test_command:
            passed = (
                subprocess.run(test_command, shell=True, cwd=workspace, capture_output=True).returncode
                == 0
            )
            if not passed:
                holds += 1
                if holds > holds_left:
                    record["gate_events"].append({"action": "exhausted"})
                    record["end_reason"] = "not_done"
                    break
                record["gate_events"].append({"action": "hold"})
                words = "was not accepted" if flavour != "other-hold" else "is refused for now"
                items.append(
                    {
                        "type": "message",
                        "role": "user",
                        "content": f"[Rules check] Your final message {words}. The check failed.",
                    }
                )
                continue
        record["end_reason"] = "done"
        break
    else:
        record["end_reason"] = "limits"
    record["exit_code"] = 0 if record["end_reason"] == "done" else 1
    record["ended_unix_ms"] = int(time.time() * 1000)
    if flavour == "other-record":
        record["extra_field"] = True
    if record_path:
        Path(record_path).write_text(json.dumps(record))
    print(json.dumps({"result": final}))
    return record["exit_code"]


if __name__ == "__main__":
    sys.exit(main())
