#!/bin/sh
# type.sh PORT TEXT — key events into the focused element
H=$($(dirname "$0")/mcp.py $1 list_windows | python3 -c 'import json,sys;print(json.dumps(json.load(sys.stdin)["windowHandles"][0]))')
T=$(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$2")
$(dirname "$0")/mcp.py $1 dispatch_key_event "{\"windowHandle\":$H,\"text\":$T}" >/dev/null && echo "typed $2"
