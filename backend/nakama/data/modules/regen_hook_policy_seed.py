#!/usr/bin/env python3
"""Regenerate hook_policy_seed.json from the hook policy catalogue.

Source: mello-backlog/plans/game-capture-hook-games/games.csv
  policy==hook    -> hook_allow_ids
  policy==no_hook -> hook_deny_ids
  hook_review     -> neither list

The lists carry the igdb_id column (mello-backlog
plans/game-identity-hook-policy.md). Deny wins over allow, and
Counter-Strike 2 (242408) is always denied. hook_enabled stays false in the
seed: exposure is flipped on from the backend (Nakama storage collection
`capture_config`, key `hook_policy`, edited in the admin tool) after review,
never by regenerating this file.

Usage:
  python3 backend/nakama/data/modules/regen_hook_policy_seed.py
"""
import csv
import json
import sys
from pathlib import Path

# mello-backlog is a sibling of the mello checkout, not a folder inside it.
# parents[4] is the mello repo; its parent is the workspace that holds both.
_HERE = Path(__file__).resolve()
_RELATIVE = Path("mello-backlog") / "plans" / "game-capture-hook-games" / "games.csv"
CSV = next(
    (c for c in (_HERE.parents[5] / _RELATIVE, _HERE.parents[4] / _RELATIVE) if c.is_file()),
    _HERE.parents[5] / _RELATIVE,
)
OUT = _HERE.parent / "hook_policy_seed.json"


CS2_IGDB_ID = 242408
POLICY_VERSION = "2026-10-06:games.csv"


def main() -> int:
    allow: set[int] = set()
    deny: set[int] = set()
    with open(CSV, encoding="utf-8") as f:
        for row in csv.DictReader(f):
            policy = (row.get("policy") or "").strip()
            igdb_id = int(row["igdb_id"])
            if igdb_id <= 0:
                raise SystemExit(f"bad igdb_id in {CSV}: {row['igdb_id']!r}")
            if policy == "hook":
                allow.add(igdb_id)
            elif policy == "no_hook":
                deny.add(igdb_id)
    deny.add(CS2_IGDB_ID)
    allow -= deny
    policy_blob = {
        "hook_enabled": False,
        "policy_version": POLICY_VERSION,
        "hook_allow_ids": sorted(allow),
        "hook_deny_ids": sorted(deny),
    }
    OUT.write_text(json.dumps(policy_blob, indent=2) + "\n", encoding="utf-8")
    print(f"wrote {OUT} allow={len(allow)} deny={len(deny)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
