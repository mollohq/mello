#!/usr/bin/env zsh
# Seed Mello backend with test users, crews, and memberships.
# Idempotent — safe to run multiple times.

set -euo pipefail

BASE="http://127.0.0.1:7350"
SERVER_KEY="mello_dev_key"
PASSWORD="password123"

# ── helpers ──────────────────────────────────────────────────────

seed_auth() {
    local email="$1" password="$2" username="$3"
    local b64
    b64=$(printf '%s:' "$SERVER_KEY" | base64)
    local resp
    resp=$(curl -sf -X POST \
        "$BASE/v2/account/authenticate/email?create=true&username=$username" \
        -H "Authorization: Basic $b64" \
        -H "Content-Type: application/json" \
        -d "{\"email\":\"$email\",\"password\":\"$password\"}")
    echo "$resp" | jq -r '.token'
}

seed_get_user_id() {
    local token="$1"
    local resp
    resp=$(curl -sf -X GET "$BASE/v2/account" \
        -H "Authorization: Bearer $token")
    echo "$resp" | jq -r '.user.id'
}

seed_ensure_group() {
    local token="$1" name="$2" desc="$3"
    local resp
    if resp=$(curl -sf -X POST "$BASE/v2/group" \
        -H "Authorization: Bearer $token" \
        -H "Content-Type: application/json" \
        -d "{\"name\":\"$name\",\"description\":\"$desc\",\"open\":true,\"max_count\":6}") 2>/dev/null; then
        local gid
        gid=$(echo "$resp" | jq -r '.id')
        echo "$gid new"
        return
    fi
    resp=$(curl -sf -X GET "$BASE/v2/group?name=$name&limit=100" \
        -H "Authorization: Bearer $token")
    local gid
    gid=$(echo "$resp" | jq -r --arg n "$name" '.groups[] | select(.name==$n) | .id' | head -1)
    if [[ -n "$gid" ]]; then
        echo "$gid exists"
        return
    fi
    echo "Cannot create or find group '$name'" >&2
    return 1
}

seed_join_group() {
    local token="$1" group_id="$2"
    if curl -sf -X POST "$BASE/v2/group/$group_id/join" \
        -H "Authorization: Bearer $token" \
        -H "Content-Type: application/json" -o /dev/null 2>/dev/null; then
        echo "joined"
    else
        echo "already in"
    fi
}

# ── preflight ────────────────────────────────────────────────────

echo ""
echo "  mello seed"
echo "  ---------"
echo ""

if ! curl -sf "$BASE/healthcheck" -o /dev/null 2>/dev/null; then
    echo "  [!] Nakama not reachable at $BASE"
    echo "      Run: docker compose up -d  (from backend/)"
    exit 1
fi

# ── users ────────────────────────────────────────────────────────

echo "  users"

typeset -A tok uid
names=(alice bob charlie diana)

for n in "${names[@]}"; do
    tok[$n]=$(seed_auth "$n@test.com" "$PASSWORD" "$n")
    uid[$n]=$(seed_get_user_id "${tok[$n]}")
    echo "    $n  ${uid[$n]}"
done

# ── crews ────────────────────────────────────────────────────────

echo ""
echo "  crews"

crew_names=("Devs" "Gamers" "Music")
crew_descs=("Development crew" "Gaming nights" "Music production")
typeset -A crew_ids

for ((i=1; i<=${#crew_names[@]}; i++)); do
    result=$(seed_ensure_group "${tok[alice]}" "${crew_names[$i]}" "${crew_descs[$i]}")
    gid=$(echo "$result" | awk '{print $1}')
    tag=$(echo "$result" | awk '{print $2}')
    crew_ids[${crew_names[$i]}]="$gid"
    echo "    ${crew_names[$i]}  $gid  ($tag)"
done

# ── memberships ──────────────────────────────────────────────────
#
#   Devs   : alice*, bob, charlie
#   Gamers : alice*, bob, diana
#   Music  : alice*, charlie, diana        (* = creator)

echo ""
echo "  memberships"

join_user=(bob     charlie bob     diana   charlie diana)
join_crew=(Devs    Devs    Gamers  Gamers  Music   Music)

for ((i=1; i<=${#join_user[@]}; i++)); do
    u="${join_user[$i]}"
    c="${join_crew[$i]}"
    tag=$(seed_join_group "${tok[$u]}" "${crew_ids[$c]}")
    echo "    $u -> $c  ($tag)"
done

# ── summary ──────────────────────────────────────────────────────

echo ""
echo "  done!"
echo ""
echo "  Credentials     <name>@test.com / $PASSWORD"
echo "  Devs            alice, bob, charlie"
echo "  Gamers          alice, bob, diana"
echo "  Music           alice, charlie, diana"
echo ""
