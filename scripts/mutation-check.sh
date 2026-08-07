#!/bin/sh
# Does the test suite actually catch regressions?
#
# A green suite proves nothing on its own — it can be green because the code
# works, or because the tests assert nothing. This introduces deliberate,
# realistic breakages one at a time and checks the suite goes red for each.
#
# Every mutation below corresponds to a bug that has either actually happened
# in this codebase or is one line away from happening.
#
# Run after adding tests, or when a suite feels suspiciously green.
# Slow (one full test run per mutation, ~30s each).
#
# Usage:  scripts/mutation-check.sh
set -eu

cd "$(dirname "$0")/.."
export CI=true

PASS=0
FAIL=0
BAK=/tmp/mello-mutation.bak

# NOTE ON `touch`: restoring a file with cp/mv gives it the backup's older
# mtime, so cargo considers it unchanged and happily reuses the artifact built
# from the *mutated* source. That silently poisons every later result. Always
# touch after restoring.
mut() {
    desc="$1"; file="$2"; pat="$3"; rep="$4"

    cp "$file" "$BAK"
    sed -i.tmp "s|$pat|$rep|" "$file" && rm -f "$file.tmp"

    if diff -q "$file" "$BAK" >/dev/null 2>&1; then
        printf '  \033[33m[STALE]\033[0m   %s\n' "$desc"
        printf '            pattern no longer matches; the code moved and this mutation needs updating\n'
        cp "$BAK" "$file"; touch "$file"
        FAIL=$((FAIL + 1))
        return
    fi

    out=$(cargo test -p mello-client --lib 2>&1 | grep -E '^test result:' | head -1 || true)
    cp "$BAK" "$file"; touch "$file"

    case "$out" in
        *FAILED*) printf '  \033[32m[caught]\033[0m  %s\n' "$desc"; PASS=$((PASS + 1)) ;;
        *ok.*)    printf '  \033[31m[MISSED]\033[0m  %s\n' "$desc"; FAIL=$((FAIL + 1)) ;;
        *)        printf '  \033[33m[ERROR]\033[0m   %s (did not compile: %s)\n' "$desc" "$out"; FAIL=$((FAIL + 1)) ;;
    esac
}

echo "▸ verifying the suite catches deliberate regressions"
echo ""

echo "screen state:"
mut "app screen gate off by one" \
    client/ui/main.slint "onboarding-step > 3): Rectangle" "onboarding-step > 4): Rectangle"

echo "auth:"
mut "OnboardingReady stops logging the user in" \
    client/src/handlers/auth.rs "ctx.app.set_logged_in(true);" "ctx.app.set_logged_in(false);"

echo "onboarding:"
mut "discovery no longer advances past step 0" \
    client/src/handlers/crew.rs "ctx.app.set_onboarding_step(1);" "ctx.app.set_onboarding_step(0);"
mut "retry loses the pending crew avatar" \
    client/src/callbacks/onboarding.rs "avatar_b64.lock().unwrap().clone()" "avatar_b64.lock().unwrap().take()"

echo "chat:"
mut "send drops the message body" \
    client/src/callbacks/chat.rs "content: text.to_string()" "content: String::new()"
mut "reply loses its parent message" \
    client/src/callbacks/chat.rs "reply_to: Some(reply_to.to_string())" "reply_to: None"

echo "crew:"
mut "select sends an empty crew id" \
    client/src/callbacks/crew.rs "crew_id: crew_id.to_string()" "crew_id: String::new()"

echo ""
if [ "$FAIL" -eq 0 ]; then
    printf '\033[32m━━━ all %s mutations caught ━━━\033[0m\n' "$PASS"
    echo "The suite fails when the code breaks, which is the only property that matters."
else
    printf '\033[31m━━━ %s caught, %s NOT caught ━━━\033[0m\n' "$PASS" "$FAIL"
    echo "A MISSED mutation is a blind spot: that code can break with the suite green."
    echo "A STALE mutation means the code moved; update the pattern here."
    exit 1
fi
