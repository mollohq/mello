#!/bin/sh
# shot.sh PORT OUT.png — screenshot the first window of an instance
H=$($(dirname "$0")/mcp.py $1 list_windows | python3 -c 'import json,sys;print(json.dumps(json.load(sys.stdin)["windowHandles"][0]))')
$(dirname "$0")/mcp.py $1 take_screenshot "{\"windowHandle\":$H}" --png $2 >/dev/null && echo "$2"
