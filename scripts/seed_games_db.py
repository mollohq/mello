#!/usr/bin/env python3
"""
Seed games.json for m3llo development.

Fetches game metadata from IGDB (covers, categories, IDs) and merges
with hardcoded exe mappings for the top competitive games.

Usage:
    export TWITCH_CLIENT_ID=your_id
    export TWITCH_CLIENT_SECRET=your_secret
    python3 seed_games_db.py

Output: games.json (ready to drop into assets/)

IGDB API requires Twitch OAuth credentials:
    https://api-docs.igdb.com/#account-creation
"""

import json
import os
import sys
import urllib.request
import urllib.parse

# -- Hardcoded exe mappings (IGDB doesn't have these) --
# key = IGDB slug, value = list of known executable names
EXE_MAP = {
    "counter-strike-2": ["cs2.exe"],
    "valorant": ["VALORANT-Win64-Shipping.exe"],
    "league-of-legends": ["League of Legends.exe"],
    "fortnite": ["FortniteClient-Win64-Shipping.exe"],
    "apex-legends": ["r5apex.exe"],
    "overwatch-2": ["Overwatch.exe"],
    "rocket-league": ["RocketLeague.exe"],
    "dota-2": ["dota2.exe"],
    "rainbow-six-siege": ["RainbowSix.exe"],
    "minecraft": ["javaw.exe", "Minecraft.Windows.exe"],
    "call-of-duty-warzone": ["cod.exe"],
    "grand-theft-auto-v": ["GTA5.exe", "PlayGTAV.exe"],
    "destiny-2": ["destiny2.exe"],
    "dead-by-daylight": ["DeadByDaylight-Win64-Shipping.exe"],
    "elden-ring": ["eldenring.exe"],
    "the-finals": ["Discovery.exe"],
    "path-of-exile": ["PathOfExile.exe", "PathOfExileSteam.exe"],
    "helldivers-2": ["helldivers2.exe"],
    "escape-from-tarkov": ["EscapeFromTarkov.exe"],
    "marvel-rivals": ["MarvelRivals-Win64-Shipping.exe"],
    "hearthstone": ["Hearthstone.exe"],
    "bloons-td-battles-2": ["btdb2_game.exe"],
}

# short display names for tight UI spaces
SHORT_NAMES = {
    "counter-strike-2": "CS2",
    "valorant": "Valorant",
    "league-of-legends": "LoL",
    "fortnite": "Fortnite",
    "apex-legends": "Apex",
    "overwatch-2": "OW2",
    "rocket-league": "Rocket",
    "dota-2": "Dota 2",
    "rainbow-six-siege": "R6",
    "minecraft": "MC",
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
    "bloons-td-battles-2": "BTD2",
}

# brand colors per game
COLORS = {
    "counter-strike-2": "#DE9B35",
    "valorant": "#FF4655",
    "league-of-legends": "#C8AA6E",
    "fortnite": "#00D4FF",
    "apex-legends": "#DA292A",
    "overwatch-2": "#FA9C1E",
    "rocket-league": "#0076FF",
    "dota-2": "#A22A20",
    "rainbow-six-siege": "#A3A3A3",
    "minecraft": "#5D8731",
    "call-of-duty-warzone": "#4CAF50",
    "grand-theft-auto-v": "#81D742",
    "destiny-2": "#E8E8E8",
    "dead-by-daylight": "#FF2D2D",
    "elden-ring": "#C4A44A",
    "the-finals": "#FF4040",
    "path-of-exile": "#B49256",
    "helldivers-2": "#FFE74C",
    "escape-from-tarkov": "#2C2C2C",
    "marvel-rivals": "#E62429",
    "hearthstone": "#CD8B36",
    "bloons-td-battles-2": "#FFE74C",
}

CATEGORY_MAP = {
    "counter-strike-2": "fps",
    "valorant": "fps",
    "league-of-legends": "moba",
    "fortnite": "br",
    "apex-legends": "br",
    "overwatch-2": "fps",
    "rocket-league": "sports",
    "dota-2": "moba",
    "rainbow-six-siege": "fps",
    "minecraft": "sandbox",
    "call-of-duty-warzone": "br",
    "grand-theft-auto-v": "sandbox",
    "destiny-2": "fps",
    "dead-by-daylight": "other",
    "elden-ring": "rpg",
    "the-finals": "fps",
    "path-of-exile": "rpg",
    "helldivers-2": "fps",
    "escape-from-tarkov": "fps",
    "marvel-rivals": "fps",
    "hearthstone": "other",
    "bloons-td-battles-2": "strategy",
}


def get_twitch_token(client_id: str, client_secret: str) -> str:
    """Get OAuth token from Twitch for IGDB API access."""
    url = "https://id.twitch.tv/oauth2/token"
    data = urllib.parse.urlencode({
        "client_id": client_id,
        "client_secret": client_secret,
        "grant_type": "client_credentials",
    }).encode()

    req = urllib.request.Request(url, data=data, method="POST")
    with urllib.request.urlopen(req) as resp:
        body = json.loads(resp.read())
        return body["access_token"]


def igdb_query(token: str, client_id: str, endpoint: str, query: str) -> list:
    """Run an IGDB API query."""
    url = f"https://api.igdb.com/v4/{endpoint}"
    req = urllib.request.Request(url, data=query.encode(), method="POST")
    req.add_header("Client-ID", client_id)
    req.add_header("Authorization", f"Bearer {token}")
    req.add_header("Content-Type", "text/plain")

    with urllib.request.urlopen(req) as resp:
        return json.loads(resp.read())


def fetch_game_by_slug(token: str, client_id: str, slug: str) -> dict | None:
    """Fetch a single game by slug from IGDB."""
    query = f'fields name, slug, cover.image_id; where slug = "{slug}"; limit 1;'
    results = igdb_query(token, client_id, "games", query)
    if results:
        return results[0]
    return None


def cover_url(image_id: str, size: str = "t_cover_big") -> str:
    """Build IGDB cover image URL."""
    return f"https://images.igdb.com/igdb/image/upload/{size}/{image_id}.png"


def icon_url(image_id: str) -> str:
    """Build IGDB icon image URL (small square)."""
    return f"https://images.igdb.com/igdb/image/upload/t_logo_med/{image_id}.png"


def main():
    client_id = os.environ.get("TWITCH_CLIENT_ID")
    client_secret = os.environ.get("TWITCH_CLIENT_SECRET")

    if not client_id or not client_secret:
        print("Error: set TWITCH_CLIENT_ID and TWITCH_CLIENT_SECRET env vars")
        print("Get credentials at: https://dev.twitch.tv/console/apps")
        sys.exit(1)

    print("Authenticating with Twitch...")
    token = get_twitch_token(client_id, client_secret)
    print("Authenticated.\n")

    games = []
    slugs = list(EXE_MAP.keys())

    for i, slug in enumerate(slugs):
        print(f"[{i+1}/{len(slugs)}] Fetching {slug}...")
        igdb_game = fetch_game_by_slug(token, client_id, slug)

        if igdb_game:
            image_id = None
            if "cover" in igdb_game and igdb_game["cover"]:
                image_id = igdb_game["cover"].get("image_id")

            entry = {
                "id": slug,
                "igdb_id": igdb_game.get("id", 0),
                "name": igdb_game.get("name", slug),
                "short_name": SHORT_NAMES.get(slug, slug),
                "exe": EXE_MAP[slug],
                "icon_url": icon_url(image_id) if image_id else "",
                "cover_url": cover_url(image_id) if image_id else "",
                "color": COLORS.get(slug, "#888888"),
                "category": CATEGORY_MAP.get(slug, "other"),
            }
        else:
            print(f"  WARNING: not found on IGDB, using local data only")
            entry = {
                "id": slug,
                "igdb_id": 0,
                "name": slug.replace("-", " ").title(),
                "short_name": SHORT_NAMES.get(slug, slug),
                "exe": EXE_MAP[slug],
                "icon_url": "",
                "cover_url": "",
                "color": COLORS.get(slug, "#888888"),
                "category": CATEGORY_MAP.get(slug, "other"),
            }

        games.append(entry)

    output = {
        "version": 1,
        "updated_at": "2026-04-03T00:00:00Z",
        "games": games,
    }

    out_path = "games.json"
    with open(out_path, "w") as f:
        json.dump(output, f, indent=4)

    print(f"\nDone. Wrote {len(games)} games to {out_path}")
    print(f"Copy to assets/games.json in the client repo.")


if __name__ == "__main__":
    main()
