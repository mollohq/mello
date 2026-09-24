#!/usr/bin/env python3
"""click_label.py PORT LABEL [N] — real click on the Nth (default 0) control whose accessible label is LABEL.

Queries control roles through Slint MCP, reads each label, clicks the element's center.
Exit 1 and list the labels on screen when nothing matches.
"""
import json, os, subprocess, sys
port, want = sys.argv[1], sys.argv[2]
n = int(sys.argv[3]) if len(sys.argv) > 3 else 0
MCP = os.path.join(os.path.dirname(os.path.abspath(__file__)), "mcp.py")
def mcp(tool, args):
    try: return json.loads(subprocess.check_output([MCP, port, tool, json.dumps(args)], stderr=subprocess.DEVNULL))
    except Exception: return {}
w = mcp("list_windows", {})["windowHandles"][0]
root = mcp("get_window_properties", {"windowHandle": w})["rootElementHandle"]
seen, hits = [], []
for role in ("Button", "Switch", "Tab", "Slider", "Combobox"):
    for h in mcp("query_element_descendants", {"elementHandle": root, "queryStack": [{"matchDescendants": True}, {"matchElementAccessibleRole": role}], "findAll": True}).get("elementHandles", []):
        label = mcp("get_element_properties", {"elementHandle": h}).get("accessibleLabel", "")
        seen.append(label)
        if label == want: hits.append(h)
if len(hits) <= n:
    print(f"NOT FOUND: {want!r} (#{n}). On screen: {sorted(set(seen))}"); sys.exit(1)
mcp("click_element", {"elementHandle": hits[n]})
print(f"clicked {want!r}")
