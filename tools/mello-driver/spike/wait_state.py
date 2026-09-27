#!/usr/bin/env python3
"""wait_state.py STATE_PORT EXPR [TIMEOUT_S] — poll /state until EXPR is true.

EXPR is a Python expression over the snapshot dict `s`, for example
"s['screen'] == 'app' and 'bob' in ' '.join(s['members'])".
Exits 0 when true, 1 on timeout (prints the last snapshot and any Error events).
"""
import json, sys, time, urllib.request
port, expr = sys.argv[1], sys.argv[2]
timeout = float(sys.argv[3]) if len(sys.argv) > 3 else 15
deadline, s = time.time() + timeout, {}
while time.time() < deadline:
    try:
        s = json.loads(urllib.request.urlopen(f"http://127.0.0.1:{port}/state", timeout=3).read())
        if eval(expr, {}, {"s": s}):
            print(f"ok: {expr}")
            sys.exit(0)
    except Exception:
        pass
    time.sleep(0.2)
print(f"TIMEOUT after {timeout}s: {expr}\nlast state: {json.dumps(s)}")
try:
    ev = json.loads(urllib.request.urlopen(f"http://127.0.0.1:{port}/events", timeout=3).read())
    for e in ev:
        if e.get("message"):
            print(f"  Error event: {e['message']}")
except Exception:
    pass
sys.exit(1)
