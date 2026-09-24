#!/usr/bin/env python3
"""Spike helper: call one Slint MCP tool. Usage: mcp.py PORT TOOL [JSON_ARGS] [--png OUT]"""
import json, sys, urllib.request, base64
port, tool = sys.argv[1], sys.argv[2]
args = json.loads(sys.argv[3]) if len(sys.argv) > 3 and not sys.argv[3].startswith("--") else {}
png = sys.argv[sys.argv.index("--png") + 1] if "--png" in sys.argv else None
method, params = ("tools/list", {}) if tool == "list" else ("tools/call", {"name": tool, "arguments": args})
req = urllib.request.Request(f"http://127.0.0.1:{port}/mcp",
    data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode(),
    headers={"Content-Type": "application/json", "Accept": "application/json, text/event-stream"})
body = urllib.request.urlopen(req, timeout=30).read().decode()
if body.startswith("event:") or "\ndata:" in body or body.startswith("data:"):
    body = "".join(l[5:] for l in body.splitlines() if l.startswith("data:"))
res = json.loads(body)
if "error" in res: print(json.dumps(res["error"])); sys.exit(1)
for c in res.get("result", {}).get("content", []):
    if c.get("type") == "image" and png:
        open(png, "wb").write(base64.b64decode(c["data"])); print(f"saved {png}")
    elif c.get("type") == "text":
        print(c["text"])
if tool == "list":
    for t in res["result"]["tools"]: print(t["name"])
