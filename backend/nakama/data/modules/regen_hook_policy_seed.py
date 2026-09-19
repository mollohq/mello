#!/usr/bin/env python3
"""Regenerate hook_policy_seed.json from the hook policy catalogue.

Source: mello-backlog/plans/game-capture-hook-games/games.csv
  policy==hook    -> hook_allow
  policy==no_hook -> hook_deny
  hook_review     -> neither list

Deny wins over allow, and cs2.exe is always denied. hook_enabled stays
false in the seed: exposure is flipped on from the backend (Nakama storage
collection `capture_config`, key `hook_policy`) after review, never by
regenerating this file.

Usage:
  python3 backend/nakama/data/modules/regen_hook_policy_seed.py
"""
import csv
import json
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[4]
CSV = REPO / "mello-backlog" / "plans" / "game-capture-hook-games" / "games.csv"
OUT = Path(__file__).resolve().parent / "hook_policy_seed.json"


def main() -> int:
    allow: set[str] = set()
    deny: set[str] = set()
    with open(CSV, encoding="utf-8") as f:
        for row in csv.DictReader(f):
            policy = (row.get("policy") or "").strip()
            for exe in (row.get("exes") or "").split(";"):
                exe = exe.strip().lower()
                if not exe:
                    continue
                if policy == "hook":
                    allow.add(exe)
                elif policy == "no_hook":
                    deny.add(exe)
    deny.add("cs2.exe")
    allow -= deny
    policy_blob = {
        "hook_enabled": False,
        "policy_version": "2026-09-19:games.csv",
        "hook_allow": sorted(allow),
        "hook_deny": sorted(deny),
    }
    OUT.write_text(json.dumps(policy_blob, indent=2) + "\n", encoding="utf-8")
    print(f"wrote {OUT} allow={len(allow)} deny={len(deny)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
