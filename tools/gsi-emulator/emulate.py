#!/usr/bin/env python3
"""Emulate Counter-Strike 2 Game State Integration (GSI) for local testing.

POSTs a recorded competitive match sequence (warmup -> live rounds -> gameover)
to mello's telemetry listener so the post-game pipeline can be exercised without
launching CS2. See specs/18-GAME-TELEMETRY.md.

The auth token is read from the same file the client writes
(%LOCALAPPDATA%/mello/telemetry_token on Windows, $XDG_CONFIG_HOME or
~/.config/mello/telemetry_token elsewhere); override with --token.

Examples:
    # One winning match
    python emulate.py
    # Three winning matches in a row (build a streak), then a loss
    python emulate.py --matches 3 --result win
    python emulate.py --result loss
"""

import argparse
import json
import os
import sys
import time
import urllib.request
from pathlib import Path

DEFAULT_PORT = 29406
DEFAULT_MAP = "de_mirage"
DEFAULT_MODE = "competitive"


def token_path() -> Path:
    if sys.platform.startswith("win"):
        base = os.environ.get("LOCALAPPDATA", str(Path.home()))
    else:
        base = os.environ.get("XDG_CONFIG_HOME") or str(Path.home() / ".config")
    return Path(base) / "mello" / "telemetry_token"


def load_token(override: str | None) -> str:
    if override:
        return override
    p = token_path()
    try:
        tok = p.read_text(encoding="utf-8").strip()
        if tok:
            return tok
    except OSError:
        pass
    sys.exit(
        f"No telemetry token found at {p}.\n"
        "Run the mello client once (so it generates one), or pass --token."
    )


def post(port: int, payload: dict) -> None:
    body = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}",
        data=body,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        urllib.request.urlopen(req, timeout=5).read()
    except Exception as e:  # noqa: BLE001 - dev tool, surface any failure
        sys.exit(f"POST failed (is the mello client running?): {e}")


def frame(token: str, mode: str, phase: str, ct: int, t: int, team: str) -> dict:
    return {
        "provider": {"name": "Counter-Strike: Global Offensive", "appid": 730},
        "auth": {"token": token},
        "map": {
            "mode": mode,
            "name": DEFAULT_MAP,
            "phase": phase,
            "team_ct": {"score": ct},
            "team_t": {"score": t},
        },
        "player": {"team": team},
    }


def play_match(token: str, port: int, mode: str, win: bool, delay: float) -> None:
    # Player on CT. Winner reaches 13; loser trails at 7.
    team = "CT"
    own_final, opp_final = (13, 7) if win else (7, 13)

    post(port, frame(token, mode, "warmup", 0, 0, team))
    time.sleep(delay)

    # Walk the score up round by round to the final.
    own, opp = 0, 0
    while own < own_final or opp < opp_final:
        if own < own_final:
            own += 1
        if opp < opp_final:
            opp += 1
        ct, t = (own, opp) if team == "CT" else (opp, own)
        post(port, frame(token, mode, "live", ct, t, team))
        time.sleep(delay)

    ct, t = (own_final, opp_final) if team == "CT" else (opp_final, own_final)
    post(port, frame(token, mode, "gameover", ct, t, team))
    print(f"  match: {'WIN' if win else 'LOSS'} {own_final}-{opp_final} on {DEFAULT_MAP}")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--matches", type=int, default=1, help="number of matches to play")
    ap.add_argument("--result", choices=["win", "loss"], default="win")
    ap.add_argument("--mode", default=DEFAULT_MODE, help="map.mode (competitive/premier/casual/...)")
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--delay", type=float, default=0.2, help="seconds between frames")
    ap.add_argument("--token", default=None, help="override auth token")
    args = ap.parse_args()

    token = load_token(args.token)
    print(f"Emulating {args.matches} {args.mode} match(es) -> 127.0.0.1:{args.port}")
    for i in range(args.matches):
        print(f"match {i + 1}/{args.matches}:")
        play_match(token, args.port, args.mode, args.result == "win", args.delay)
        time.sleep(args.delay)
    print("done. Stop the emulated game (close CS2) in the client to finalize the session.")


if __name__ == "__main__":
    main()
