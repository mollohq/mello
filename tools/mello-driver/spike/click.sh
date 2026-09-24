#!/bin/sh
# click.sh PORT QUALIFIED_ID [N] — real click on the Nth (default 0) element with that ID
P=$1; ID=$2; N=${3:-0}
H=$($(dirname "$0")/mcp.py $P list_windows | python3 -c 'import json,sys;print(json.dumps(json.load(sys.stdin)["windowHandles"][0]))')
E=$($(dirname "$0")/mcp.py $P find_elements_by_id "{\"windowHandle\":$H,\"elementsId\":\"$ID\"}" | python3 -c "import json,sys;h=json.load(sys.stdin)['elementHandles'];print(json.dumps(h[$N]) if len(h)>$N else '')")
[ -z "$E" ] && { echo "NOT FOUND: $ID"; exit 1; }
$(dirname "$0")/mcp.py $P click_element "{\"elementHandle\":$E}" >/dev/null && echo "clicked $ID"
