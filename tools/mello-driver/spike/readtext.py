#!/usr/bin/env python3
"""readtext.py PORT SUBSTRING — print accessible labels of visible Text elements containing SUBSTRING"""
import json, os, subprocess, sys
port, sub = sys.argv[1], sys.argv[2]
def mcp(tool, args):
    try: return json.loads(subprocess.check_output([os.path.join(os.path.dirname(os.path.abspath(__file__)), "mcp.py"), port, tool, json.dumps(args)], stderr=subprocess.DEVNULL))
    except Exception: return {}
w = mcp("list_windows", {})["windowHandles"][0]
root = mcp("get_window_properties", {"windowHandle": w})["rootElementHandle"]
hits = []
for q in ({"matchElementTypeName": "Text"}, {"matchElementTypeNameOrBase": "TextInput"}):
    hits += mcp("query_element_descendants", {"elementHandle": root, "queryStack": [{"matchDescendants": True}, q], "findAll": True}).get("elementHandles", [])
for h in hits:
    p = mcp("get_element_properties", {"elementHandle": h})
    label = p.get("accessibleLabel", "") or p.get("accessibleValue", "")
    if sub in label: print(label)
