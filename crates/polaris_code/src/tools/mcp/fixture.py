"""Local JSON-RPC MCP fixture; never contacts a model or external service."""

import json
import os
import sys
import time


def respond(request, result):
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)


if os.environ.get("FIXTURE_PID"):
    with open(os.environ["FIXTURE_PID"], "w") as file:
        file.write(str(os.getpid()))

for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if "id" not in request:
        continue
    if method == "initialize":
        if os.environ.get("FIXTURE_MODE") == "init-timeout":
            time.sleep(60)
        respond(request, {
            "protocolVersion": request["params"]["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "local-fixture", "version": "1"},
        })
    elif method == "tools/list":
        mode = os.environ.get("FIXTURE_MODE", "")
        if mode == "discovery-timeout":
            time.sleep(60)
        names = ["echo", "error", "timeout", "crash", "large", "environment", "rpc_error"]
        if mode == "bad-name":
            names.append("bad__name")
        if mode == "duplicate":
            names.append("echo")
        result = {"tools": [{
            "name": name, "description": "Local fixture " + name,
            "inputSchema": {"type": "object", "properties": {}},
            # An untrusted annotation must never grant read-only permissions.
            "annotations": {"readOnlyHint": True},
        } for name in names]}
        if mode == "pagination":
            if request.get("params", {}).get("cursor") is None:
                result["tools"] = result["tools"][:2]
                result["nextCursor"] = "next"
            else:
                result["tools"] = result["tools"][2:]
        if mode in ("too-many-tools", "max-tools"):
            count = 257 if mode == "too-many-tools" else 256
            result["tools"] = [dict(result["tools"][0], name="tool" + str(i)) for i in range(count)]
        if mode in ("too-many-pages", "max-pages"):
            result["tools"] = []
            cursor = int(request.get("params", {}).get("cursor", "0"))
            if mode == "too-many-pages" or cursor < 31:
                result["nextCursor"] = str(cursor + 1)
        if mode in ("metadata-limit", "schema-limit"):
            result["tools"] = [dict(result["tools"][0], name="tool" + str(request.get("params", {}).get("cursor", "0")))]
            if mode == "metadata-limit":
                result["tools"][0]["description"] = "x" * 40_000
            else:
                result["tools"][0]["inputSchema"]["properties"] = {"field": {"type": "string", "description": "x" * 40_000}}
            cursor = int(request.get("params", {}).get("cursor", "0"))
            result["nextCursor"] = str(cursor + 1)
        if mode == "repeated-cursor":
            result["nextCursor"] = "same"
        respond(request, result)
    elif method == "tools/call":
        name = request["params"]["name"]
        value = request["params"].get("arguments", {})
        if name == "timeout":
            time.sleep(60)
        elif name == "crash":
            sys.exit(7)
        elif name == "large":
            value = "é" * 40_000
        elif name == "environment":
            value = dict(os.environ)
        elif name == "rpc_error":
            print(json.dumps({
                "jsonrpc": "2.0", "id": request["id"],
                "error": {"code": -32603, "message": "private server diagnostic"},
            }), flush=True)
            continue
        respond(request, {
            "content": [{"type": "text", "text": json.dumps(value, ensure_ascii=False)}],
            "isError": name == "error",
        })
