#!/bin/sh
# Phase 0 invite journey (plans/E2E-QA.md §6): two fresh users, real clicks,
# local stack. Exit 0 = pass. Needs a build with `--features development,e2e`
# and SLINT_EMIT_DEBUG_INFO=1, and a local Nakama on 127.0.0.1:7350.
set -eu
S=$(cd "$(dirname "$0")" && pwd)
RUN=$(date +%s)
export MELLO_E2E_RUN_DIR=${MELLO_E2E_RUN_DIR:-/tmp/mello-e2e/$RUN}
A=9401; B=9402; AS=$((A+100)); BS=$((B+100))
cleanup() { kill "$(cat "$MELLO_E2E_RUN_DIR/alice/pid")" "$(cat "$MELLO_E2E_RUN_DIR/bob/pid" 2>/dev/null)" 2>/dev/null || true; }
trap cleanup EXIT
step() { echo "▸ $*"; }

step "alice: fresh install, create crew"
"$S/launch.sh" alice $A
"$S/wait_state.py" $AS "s['screen'] == 'onboarding' and s['onboarding_step'] == 1" 30
"$S/click.sh" $A CreateCrewCard::create-touch
"$S/click.sh" $A NewCrewModal::name-input
"$S/type.sh" $A "Night Owls $RUN"
"$S/click.sh" $A NewCrewModal::create-touch
"$S/wait_state.py" $AS "s['onboarding_step'] == 2" 10
"$S/click.sh" $A AvatarCard::av-touch 1
"$S/click.sh" $A InputField::ti
"$S/type.sh" $A "alice$RUN"
"$S/click.sh" $A AccentButton::btn-touch
"$S/wait_state.py" $AS "s['onboarding_step'] == 3 and s['logged_in']" 30
"$S/click.sh" $A Onboarding::skip-touch
"$S/wait_state.py" $AS "s['screen'] == 'app' and s['crews'] == ['Night Owls $RUN']" 15

step "alice: read the invite link from the screen"
"$S/click.sh" $A InviteCard::invite-btn-touch
LINK=""; i=0
while [ -z "$LINK" ] && [ $i -lt 20 ]; do
  LINK=$("$S/readtext.py" $A "/join/" | head -1); i=$((i+1))
done
[ -n "$LINK" ] || { echo "✗ no invite link on screen"; exit 1; }
CODE=${LINK##*/join/}
echo "  code=$CODE"

step "bob: cold start from the deep link"
"$S/launch.sh" bob $B "mello://join/$CODE"
"$S/wait_state.py" $BS "s['screen'] == 'onboarding' and s['onboarding_step'] == 1" 30
# Known issue: the invited crew is not offered at step 1 (see plans/E2E-QA.md §14).
"$S/click.sh" $B CreateCrewCard::create-touch
"$S/click.sh" $B NewCrewModal::name-input
"$S/type.sh" $B "Bob Placeholder $RUN"
"$S/click.sh" $B NewCrewModal::create-touch
"$S/wait_state.py" $BS "s['onboarding_step'] == 2" 10
"$S/click.sh" $B AvatarCard::av-touch 2
"$S/click.sh" $B InputField::ti
"$S/type.sh" $B "bob$RUN"
"$S/click.sh" $B AccentButton::btn-touch
"$S/wait_state.py" $BS "s['join_crew_modal_open'] and s['join_crew_name'] == 'Night Owls $RUN'" 30

step "bob: join the invited crew"
"$S/click.sh" $B JoinCrewModal::join-touch
"$S/wait_state.py" $BS "'Night Owls $RUN' in s['crews']" 15

step "alice: sees bob in the crew"
"$S/wait_state.py" $AS "any(m == 'bob$RUN' for m in s['members'])" 15

echo "✓ invite journey passed ($MELLO_E2E_RUN_DIR)"
