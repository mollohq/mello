#!/usr/bin/env python3
"""
Build the game catalogue artifacts from the IGDB data dumps.

Replaces the hand-maintained exe/short-name/colour dicts in seed_games_db.py
with a derived build over IGDB's daily CSV dumps. Two artifacts come out, split
by how often they change and how they reach the client (plans/GAME-SENSING-V2.md
§2.2):

  head.bin          ~2k most-played games, full metadata. Committed to the repo
                    and include_bytes!'d — the installer grows by ~57KB and
                    popular games resolve instantly, offline, with no network.

  appid_index.bin   steam_appid -> igdb_id for every game IGDB knows (~135k).
                    Served by the backend and fetched at runtime, so catalogue
                    freshness is decoupled from app releases.

Usage:
    python build_catalogue.py                # build both artifacts
    python build_catalogue.py --check-schema # verify IGDB's schema is unchanged
    python build_catalogue.py --head-size N  # default 2000

Credentials: TWITCH_CLIENT_ID / TWITCH_CLIENT_SECRET, or a JSON file at
scripts/igdb_credentials.json (gitignored) with client_id / client_secret.

Stdlib only — no third-party dependencies.
"""

import argparse
import csv
import gzip
import json
import os
import struct
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
WORK = os.path.join(HERE, ".catalogue_work")
OUT_DIR = os.path.join(REPO, "client", "assets", "catalogue")
SCHEMA_LOCK = os.path.join(HERE, "catalogue_schema.json")

# IGDB platform ids we ship. 96.7% of Mac titles are also on Windows, so one
# artifact serves both desktop platforms for +0.6% rows.
PLATFORM_WINDOWS = "6"
PLATFORM_MAC = "14"
DESKTOP = {PLATFORM_WINDOWS, PLATFORM_MAC}

# Which game_types can a user actually launch as a distinct thing?
#
# Filtering to "Main Game" (type 0) looks right and is not: Minecraft's PC
# entry is a Port, Resident Evil 2 is a Remake, Half-Life 2: Episode Two is a
# Standalone Expansion. Excluding those silently loses top-tier games, so this
# is a denylist of types that genuinely cannot be launched on their own —
# they have no install of their own and ride inside a parent game.
NON_LAUNCHABLE_TYPES = {
    "1",   # DLC
    "2",   # Expansion
    "3",   # Bundle
    "6",   # Episode
    "7",   # Season
    "13",  # Pack / Addon
    "14",  # Update
}

ENDPOINTS = [
    "games",
    "external_games",
    "external_game_sources",
    "covers",
    "alternative_names",
    "game_types",
    "platforms",
]

MAGIC_HEAD = b"MHD1"
MAGIC_INDEX = b"MAI1"

# Hand-picked short names win over derivation. Seeded from the 25 curated
# entries in the v1 games.json — derivation cannot know that League of Legends
# is "LoL" and not "LL".
SHORT_NAME_OVERRIDES = {
    "counter-strike-2": "CS2",
    "valorant": "Valorant",
    "league-of-legends": "LoL",
    "fortnite": "Fortnite",
    "apex-legends": "Apex",
    "overwatch-2": "OW2",
    "rocket-league": "Rocket",
    "dota-2": "Dota 2",
    "rainbow-six-siege": "R6",
    # IGDB splits Minecraft across editions; both are separately launchable and
    # both should badge as "MC". The bare "minecraft" slug does not exist.
    "minecraft-java-edition": "MC",
    "minecraft--1": "MC",
    "call-of-duty-warzone": "Warzone",
    "grand-theft-auto-v": "GTA V",
    "destiny-2": "Destiny",
    "dead-by-daylight": "DBD",
    "elden-ring": "Elden",
    "the-finals": "Finals",
    "path-of-exile": "PoE",
    "helldivers-2": "HD2",
    "escape-from-tarkov": "Tarkov",
    "marvel-rivals": "Rivals",
    "hearthstone": "HS",
    "legends-of-runeterra": "LoR",
    "starcraft-ii": "SC2",
    "world-of-warcraft": "WoW",
    "teamfight-tactics": "TFT",
    "player-unknowns-battlegrounds": "PUBG",
    "baldurs-gate-3": "BG3",
}

# Words that never contribute an initial.
STOPWORDS = {"of", "the", "and", "a", "an", "de", "la"}

csv.field_size_limit(min(sys.maxsize, 2**31 - 1))


# ------------------------------------------------------------------ IGDB API


def credentials():
    cid = os.environ.get("TWITCH_CLIENT_ID")
    secret = os.environ.get("TWITCH_CLIENT_SECRET")
    if cid and secret:
        return cid, secret
    path = os.path.join(HERE, "igdb_credentials.json")
    if os.path.exists(path):
        with open(path, encoding="utf-8") as f:
            blob = json.load(f)
        return blob["client_id"], blob["client_secret"]
    sys.exit(
        "No IGDB credentials. Set TWITCH_CLIENT_ID / TWITCH_CLIENT_SECRET, or "
        f"create {path}"
    )


def access_token(cid, secret):
    body = urllib.parse.urlencode(
        {"client_id": cid, "client_secret": secret, "grant_type": "client_credentials"}
    ).encode()
    req = urllib.request.Request("https://id.twitch.tv/oauth2/token", data=body, method="POST")
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.load(resp)["access_token"]


def api(path, cid, tok):
    req = urllib.request.Request(f"https://api.igdb.com/v4{path}")
    req.add_header("Client-ID", cid)
    req.add_header("Authorization", f"Bearer {tok}")
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            return json.load(resp)
    except urllib.error.HTTPError as e:
        sys.exit(f"IGDB {path} -> HTTP {e.code}\n{e.read().decode('utf-8', 'replace')[:400]}")


def fetch_dumps(endpoints):
    """Download each dump and return {endpoint: schema_version}."""
    cid, secret = credentials()
    tok = access_token(cid, secret)
    os.makedirs(WORK, exist_ok=True)
    versions = {}
    for ep in endpoints:
        meta = api(f"/dumps/{ep}", cid, tok)
        versions[ep] = str(meta.get("schema_version", ""))
        out = os.path.join(WORK, f"{ep}.csv")
        stamp = os.path.join(WORK, f"{ep}.updated_at")
        updated = str(meta.get("updated_at", ""))
        if os.path.exists(out) and os.path.exists(stamp):
            with open(stamp, encoding="utf-8") as f:
                if f.read().strip() == updated:
                    print(f"  {ep:<24} cached")
                    continue
        print(f"  {ep:<24} downloading {meta.get('size_bytes',0)/1048576:.1f} MB ...", end="", flush=True)
        # The S3 URL is presigned and expires in 5 minutes — fetch immediately.
        urllib.request.urlretrieve(meta["s3_url"], out)
        with open(out, "rb") as f:
            gzipped = f.read(2) == b"\x1f\x8b"
        if gzipped:
            with gzip.open(out, "rb") as gz, open(out + ".tmp", "wb") as plain:
                while chunk := gz.read(1 << 20):
                    plain.write(chunk)
            os.replace(out + ".tmp", out)
        with open(stamp, "w", encoding="utf-8") as f:
            f.write(updated)
        print(" done")
    return versions


def check_schema(versions):
    """IGDB bumps schema_version when the dump structure changes. Importing a
    shifted schema silently produces a wrong catalogue, so fail loudly."""
    if not os.path.exists(SCHEMA_LOCK):
        with open(SCHEMA_LOCK, "w", encoding="utf-8") as f:
            json.dump(versions, f, indent=2, sort_keys=True)
        print(f"Wrote initial schema lock to {SCHEMA_LOCK}")
        return
    with open(SCHEMA_LOCK, encoding="utf-8") as f:
        locked = json.load(f)
    drift = {ep: (locked.get(ep), got) for ep, got in versions.items() if locked.get(ep) != got}
    if drift:
        print("IGDB dump schema changed:", file=sys.stderr)
        for ep, (was, now) in drift.items():
            print(f"  {ep}: {was} -> {now}", file=sys.stderr)
        sys.exit(
            "Refusing to build against an unverified schema. Review the field "
            f"changes, then update {SCHEMA_LOCK}."
        )
    print("Schema lock matches.")


# ------------------------------------------------------------------- reading


def rows(name):
    with open(os.path.join(WORK, f"{name}.csv"), encoding="utf-8", newline="") as f:
        yield from csv.DictReader(f)


def id_list(cell):
    """IGDB array cells look like {1,2,3}."""
    if not cell:
        return []
    return [p for p in cell.strip("{}[]").replace('"', "").split(",") if p.strip()]


def derive_short_name(name, slug):
    """A badge-sized label. Curated overrides win; otherwise take initials of
    significant words, keeping trailing numerals ("Counter-Strike 2" -> "CS2")."""
    if slug in SHORT_NAME_OVERRIDES:
        return SHORT_NAME_OVERRIDES[slug]
    if len(name) <= 7:
        return name
    words = [w for w in name.replace("-", " ").replace(":", " ").split() if w]
    significant = [w for w in words if w.lower() not in STOPWORDS]
    if not significant:
        return name[:7]
    if len(significant) == 1:
        # One real word: use the word, not the whole name. Truncating the name
        # instead turned "The Saboteur" into "The Sab".
        return significant[0][:8]
    out = ""
    for w in significant:
        out += w if w.isdigit() or (len(w) <= 2 and w.isupper()) else w[0].upper()
    return out[:8] if out else name[:7]


# ------------------------------------------------------------------- packing


class StringBlob:
    """Deduplicated UTF-8 blob; returns (offset, len) capped at 255 bytes."""

    def __init__(self):
        self.buf = bytearray()
        self.seen = {}

    def add(self, s):
        if not s:
            return (0, 0)
        b = s.encode("utf-8")[:255]
        if b in self.seen:
            return self.seen[b]
        entry = (len(self.buf), len(b))
        self.buf.extend(b)
        self.seen[b] = entry
        return entry


def build_head(games, covers, popularity, size):
    """head.bin — the N most-played games with full display metadata.

    Layout (little-endian):
        magic "MHD1" | count u32 | strings_off u32
        count x 24-byte record, sorted by igdb_id for binary search:
            igdb_id u32, name_off u32, slug_off u32, short_off u32,
            cover_off u32, name_len u8, slug_len u8, short_len u8, cover_len u8
        string blob (offsets are relative to strings_off)
    """
    ranked = sorted(games, key=lambda g: popularity.get(g, 0), reverse=True)[:size]
    ranked.sort(key=int)  # binary-searchable by igdb_id

    blob = StringBlob()
    records = bytearray()
    for gid in ranked:
        name, slug = games[gid]
        n_off, n_len = blob.add(name)
        s_off, s_len = blob.add(slug)
        sh_off, sh_len = blob.add(derive_short_name(name, slug))
        c_off, c_len = blob.add(covers.get(gid, ""))
        records.extend(
            struct.pack(
                "<IIIIIBBBB", int(gid), n_off, s_off, sh_off, c_off, n_len, s_len, sh_len, c_len
            )
        )

    header = MAGIC_HEAD + struct.pack("<II", len(ranked), 12 + len(records))
    return header + bytes(records) + bytes(blob.buf), len(ranked)


def build_appid_index(steam):
    """appid_index.bin — steam_appid -> igdb_id, sorted for binary search.

    Layout: magic "MAI1" | count u32 | count x (appid u32, igdb_id u32)

    Flat rather than delta-encoded so the client can binary-search the file as
    it sits; the gzipped copy alongside is what goes over the wire.
    """
    pairs = sorted(steam.items())
    body = b"".join(struct.pack("<II", appid, gid) for appid, gid in pairs)
    return MAGIC_INDEX + struct.pack("<I", len(pairs)) + body, len(pairs)


# ---------------------------------------------------------------------- main


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--check-schema", action="store_true", help="verify schema and exit")
    ap.add_argument("--head-size", type=int, default=2000, help="games in head.bin (default 2000)")
    args = ap.parse_args()

    print("Fetching IGDB dumps ...")
    versions = fetch_dumps(ENDPOINTS)
    check_schema(versions)
    if args.check_schema:
        return

    print("Reading games ...")
    games, popularity = {}, {}
    for g in rows("games"):
        if g.get("game_type") in NON_LAUNCHABLE_TYPES:
            continue
        if not (set(id_list(g.get("platforms"))) & DESKTOP):
            continue
        try:
            pop = int(g.get("total_rating_count") or 0) + int(g.get("follows") or 0)
        except ValueError:
            pop = 0
        games[g["id"]] = (g.get("name", ""), g.get("slug", ""))
        popularity[g["id"]] = pop
    print(f"  {len(games):,} desktop main games")

    covers = {}
    for c in rows("covers"):
        if c.get("game") in games and c.get("image_id"):
            covers[c["game"]] = c["image_id"]
    print(f"  {len(covers):,} with cover art")

    srcname = {s["id"]: s["name"] for s in rows("external_game_sources")}
    steam = {}
    for e in rows("external_games"):
        gid = e.get("game")
        if gid not in games or srcname.get(e.get("external_game_source")) != "Steam":
            continue
        uid = (e.get("uid") or "").strip()
        if uid.isdigit() and int(uid) < 2**32:
            steam[int(uid)] = int(gid)
    print(f"  {len(steam):,} with a Steam appid")

    os.makedirs(OUT_DIR, exist_ok=True)

    head, head_count = build_head(games, covers, popularity, args.head_size)
    head_path = os.path.join(OUT_DIR, "head.bin")
    with open(head_path, "wb") as f:
        f.write(head)

    index, index_count = build_appid_index(steam)
    index_path = os.path.join(OUT_DIR, "appid_index.bin")
    with open(index_path, "wb") as f:
        f.write(index)
    with open(index_path + ".gz", "wb") as f:
        f.write(gzip.compress(index, 9))

    manifest = {
        "built_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "schema_versions": versions,
        "head_count": head_count,
        "index_count": index_count,
        "head_bytes": len(head),
        "index_bytes": len(index),
    }
    with open(os.path.join(OUT_DIR, "manifest.json"), "w", encoding="utf-8") as f:
        json.dump(manifest, f, indent=2, sort_keys=True)

    gz = len(gzip.compress(head, 9))
    print(f"\nhead.bin        {head_count:>7,} games   {len(head)/1024:>8.1f} KB  ({gz/1024:.1f} KB gz, bundled)")
    print(f"appid_index.bin {index_count:>7,} appids  {len(index)/1024:>8.1f} KB  "
          f"({len(gzip.compress(index,9))/1024:.1f} KB gz, served)")
    print(f"\nWrote {OUT_DIR}")


if __name__ == "__main__":
    main()
