# Sourced by check.sh and check-full.sh. POSIX sh.
#
# Bob's PC and Mac are also the self-hosted CI runners. A local gate that
# runs while a CI job builds on the same machine slows both: the PR Checks
# Windows job went from 16 min to 31-41 min. When a runner job is running,
# this lowers the priority of the calling shell (every child inherits it)
# and uses half the cores for cargo.
#
# MELLO_CHECK_IGNORE_CI=1 skips this.

ci_job_running() {
    case "$(uname -s)" in
        MINGW*|MSYS*|CYGWIN*)
            tasklist //FI "IMAGENAME eq Runner.Worker.exe" 2>/dev/null | grep -qi 'Runner.Worker'
            ;;
        *)
            pgrep -f Runner.Worker >/dev/null 2>&1
            ;;
    esac
}

ci_cores() {
    if [ -n "${NUMBER_OF_PROCESSORS:-}" ]; then
        echo "$NUMBER_OF_PROCESSORS"
    elif command -v nproc >/dev/null 2>&1; then
        nproc
    else
        sysctl -n hw.ncpu 2>/dev/null || echo 2
    fi
}

if [ -z "${MELLO_CHECK_IGNORE_CI:-}" ] && ci_job_running; then
    CI_YIELD_JOBS=$(( $(ci_cores) / 2 ))
    [ "$CI_YIELD_JOBS" -ge 1 ] || CI_YIELD_JOBS=1
    export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-$CI_YIELD_JOBS}"
    printf '\033[33mA CI job is running on this machine; running at low priority with %s jobs. Set MELLO_CHECK_IGNORE_CI=1 to skip this.\033[0m\n' "$CARGO_BUILD_JOBS"
    case "$(uname -s)" in
        MINGW*|MSYS*|CYGWIN*)
            # $$ is the MSYS PID; Windows knows this shell by its winpid.
            if [ -r "/proc/$$/winpid" ]; then
                powershell.exe -NoProfile -NonInteractive -Command \
                    "(Get-Process -Id $(cat "/proc/$$/winpid")).PriorityClass = 'BelowNormal'" \
                    >/dev/null 2>&1 || true
            fi
            ;;
        *)
            renice -n 10 -p $$ >/dev/null 2>&1 || true
            ;;
    esac
fi
