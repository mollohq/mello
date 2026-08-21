//! The game catalogue: IGDB identity and display metadata, derived from the
//! daily data dumps by `scripts/build_catalogue.py`.
//!
//! Two artifacts, split by how much metadata each carries (see
//! `plans/GAME-SENSING-V2.md` §2.2). Both are compile-time bundled via
//! `include_bytes!` and validated on load:
//!
//! * **head** — the ~2,000 most-played games with full display metadata.
//!   Popular games resolve instantly and offline.
//! * **appid index** — `steam_appid -> igdb_id` for every game IGDB knows
//!   (~137k). Every Steam game resolves to an IGDB id offline from first
//!   launch.
//!
//! The split exists because the client does not need *names* for 183k games —
//! it needs to know **which game** an install is, since the igdb_id keys
//! sessions and stats. Names and covers are only needed for games actually
//! played, which is a few dozen, and those are fetched lazily and cached.
//!
//! Both files are read-only, little-endian, and validated on load: a truncated
//! or corrupt artifact yields `None` rather than a panic or a wrong game.

/// `head.bin`, built by `scripts/build_catalogue.py` and refreshed by rerunning
/// it. Costs ~136KB of binary size; see §2.1 for why that budget is tight.
const HEAD_BYTES: &[u8] = include_bytes!("../../client/assets/catalogue/head.bin");

/// `appid_index.bin`. Bundled rather than fetched: at ~537KB delta-encoded the
/// installer cost is small enough that shipping it beats a download path, and
/// every Steam game then resolves to an IGDB id offline from first launch.
const INDEX_BYTES: &[u8] = include_bytes!("../../client/assets/catalogue/appid_index.bin");

const MAGIC_HEAD: &[u8; 4] = b"MHD2";
const MAGIC_INDEX: &[u8; 4] = b"MAI2";
/// igdb_id + five string offsets + five lengths + padding.
const HEAD_RECORD_LEN: usize = 32;
const HEAD_HEADER_LEN: usize = 16;
/// exe_off, shape_off, record_idx, exe_len, shape_len, pad.
const EXE_ENTRY_LEN: usize = 16;
/// (steam_appid u32, igdb_id u32)
const INDEX_PAIR_LEN: usize = 8;
const INDEX_HEADER_LEN: usize = 8;

/// A game's display metadata, borrowed straight out of the artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogueEntry<'a> {
    pub igdb_id: u32,
    /// Stable id for the ledger, `user_game_stats` and the spec-18 telemetry
    /// adapters. Defaults to the IGDB slug, but the curated table overrides it
    /// where IGDB disagrees — Minecraft is `minecraft-java-edition` upstream
    /// while the adapter has always keyed on `minecraft`, and changing that
    /// would orphan every stat already stored under it.
    pub game_id: &'a str,
    pub name: &'a str,
    pub slug: &'a str,
    /// Badge-sized label, e.g. "CS2". Derived at build time with a curated
    /// override table.
    pub short_name: &'a str,
    /// IGDB cover `image_id`; empty when the game has no art. Combine with
    /// the image CDN to fetch. Not an icon — see §8.1.
    pub cover_image: &'a str,
}

/// Read one LEB128 varint, returning its value and the next offset.
fn read_varint(bytes: &[u8], mut pos: usize) -> Option<(u32, usize)> {
    let mut value: u32 = 0;
    let mut shift = 0u32;
    loop {
        let byte = *bytes.get(pos)?;
        pos += 1;
        // A varint wider than 5 bytes cannot fit a u32; treat it as corruption
        // rather than silently wrapping.
        if shift > 28 {
            return None;
        }
        value |= u32::from(byte & 0x7F).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some((value, pos));
        }
        shift += 7;
    }
}

fn u32_at(bytes: &[u8], off: usize) -> Option<u32> {
    let slice = bytes.get(off..off + 4)?;
    Some(u32::from_le_bytes(slice.try_into().ok()?))
}

/// The bundled head: display metadata for the most-played games.
pub struct Head {
    bytes: &'static [u8],
    count: usize,
    strings_off: usize,
    exe_count: usize,
    /// Lowercased exe basename -> every curated entry claiming it. A name can
    /// map to several games (`hl2.exe` is a dozen Valve titles), which is what
    /// `path_contains` disambiguates.
    by_exe: std::collections::HashMap<&'static str, Vec<usize>>,
}

impl Head {
    /// Parse the compiled-in artifact. Returns `None` if it is not a valid
    /// head — a build that shipped a corrupt file degrades to "no bundled
    /// metadata" rather than crashing on startup.
    pub fn bundled() -> Option<Self> {
        Self::parse(HEAD_BYTES)
    }

    fn parse(bytes: &'static [u8]) -> Option<Self> {
        if bytes.len() < HEAD_HEADER_LEN || &bytes[0..4] != MAGIC_HEAD {
            return None;
        }
        let count = u32_at(bytes, 4)? as usize;
        let strings_off = u32_at(bytes, 8)? as usize;
        let exe_count = u32_at(bytes, 12)? as usize;
        // Records and the exe table must fit between the header and the string
        // blob, and the blob must start inside the file.
        let records_end = HEAD_HEADER_LEN.checked_add(count.checked_mul(HEAD_RECORD_LEN)?)?;
        let exe_end = records_end.checked_add(exe_count.checked_mul(EXE_ENTRY_LEN)?)?;
        if exe_end > strings_off || strings_off > bytes.len() {
            return None;
        }
        let mut head = Head {
            bytes,
            count,
            strings_off,
            exe_count,
            by_exe: std::collections::HashMap::new(),
        };
        head.by_exe = head.build_exe_map();
        Some(head)
    }

    /// The exe table is tiny (tens of entries), so it is indexed once at load
    /// rather than searched per scan.
    fn build_exe_map(&self) -> std::collections::HashMap<&'static str, Vec<usize>> {
        let mut map: std::collections::HashMap<&'static str, Vec<usize>> =
            std::collections::HashMap::new();
        let base = HEAD_HEADER_LEN + self.count * HEAD_RECORD_LEN;
        for i in 0..self.exe_count {
            let o = base + i * EXE_ENTRY_LEN;
            let Some(e) = self.bytes.get(o..o + EXE_ENTRY_LEN) else {
                continue;
            };
            let exe_off = u32::from_le_bytes(e[0..4].try_into().unwrap_or_default());
            let exe_len = e[12];
            let exe = self.static_string(exe_off, exe_len);
            if !exe.is_empty() {
                map.entry(exe).or_default().push(i);
            }
        }
        map
    }

    /// Strings live inside the compiled-in artifact, so they outlive the
    /// `Head` and can be handed out as `&'static str`.
    fn static_string(&self, off: u32, len: u8) -> &'static str {
        if len == 0 {
            return "";
        }
        let start = self.strings_off + off as usize;
        self.bytes
            .get(start..start + len as usize)
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("")
    }

    /// Resolve a running process to a curated game.
    ///
    /// Rung 0 of the resolution ladder, and checked before anything
    /// launcher-derived precisely because it is launcher-independent: it
    /// catches Hearthstone whether Battle.net put it in the default location
    /// or not, and League even if the Riot client installed it to another
    /// drive. `exe_path` is the full path, used only to disambiguate shared
    /// runtime hosts.
    pub fn lookup_exe(&self, exe: &str, exe_path: &str) -> Option<CatalogueEntry<'_>> {
        let key = exe.to_ascii_lowercase();
        let candidates = self.by_exe.get(key.as_str())?;
        let path = exe_path.to_ascii_lowercase();
        let base = HEAD_HEADER_LEN + self.count * HEAD_RECORD_LEN;

        // A guarded entry wins only if its marker is in the path; an unguarded
        // one is the fallback. Without this, javaw.exe would report every Java
        // application as Minecraft.
        let mut unguarded = None;
        for &i in candidates {
            let o = base + i * EXE_ENTRY_LEN;
            let e = self.bytes.get(o..o + EXE_ENTRY_LEN)?;
            let shape_off = u32::from_le_bytes(e[4..8].try_into().ok()?);
            let record_idx = u32::from_le_bytes(e[8..12].try_into().ok()?) as usize;
            let shape_len = e[13];
            let shape = self.static_string(shape_off, shape_len);
            if shape.is_empty() {
                unguarded.get_or_insert(record_idx);
            } else if path.contains(shape) {
                return self.entry_at(record_idx);
            }
        }
        unguarded.and_then(|i| self.entry_at(i))
    }

    pub fn exe_count(&self) -> usize {
        self.exe_count
    }

    /// Look up by the stable `game_id` carried on ledger events and stats.
    ///
    /// Linear over the records, which is fine for a few thousand and avoids a
    /// second index; callers hit this once per card render, not per frame.
    pub fn by_game_id(&self, game_id: &str) -> Option<CatalogueEntry<'_>> {
        if game_id.is_empty() {
            return None;
        }
        (0..self.count)
            .filter_map(|i| self.entry_at(i))
            .find(|e| e.game_id == game_id)
    }

    /// Case-insensitive lookup by display name, for legacy events that carry
    /// only a name (stream sessions, pre-`game_id` game sessions).
    pub fn by_name(&self, name: &str) -> Option<CatalogueEntry<'_>> {
        if name.is_empty() {
            return None;
        }
        (0..self.count)
            .filter_map(|i| self.entry_at(i))
            .find(|e| e.name.eq_ignore_ascii_case(name))
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn string_at(&self, off: u32, len: u8) -> &str {
        if len == 0 {
            return "";
        }
        let start = self.strings_off + off as usize;
        self.bytes
            .get(start..start + len as usize)
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("")
    }

    fn entry_at(&self, i: usize) -> Option<CatalogueEntry<'_>> {
        let o = HEAD_HEADER_LEN + i * HEAD_RECORD_LEN;
        let rec = self.bytes.get(o..o + HEAD_RECORD_LEN)?;
        let g = |k: usize| u32::from_le_bytes(rec[k..k + 4].try_into().ok().unwrap_or_default());
        Some(CatalogueEntry {
            igdb_id: g(0),
            name: self.string_at(g(4), rec[24]),
            slug: self.string_at(g(8), rec[25]),
            short_name: self.string_at(g(12), rec[26]),
            cover_image: self.string_at(g(16), rec[27]),
            game_id: self.string_at(g(20), rec[28]),
        })
    }

    /// Look up by IGDB id. Records are sorted by id at build time, so this is
    /// a binary search over the compiled-in bytes with no allocation.
    pub fn get(&self, igdb_id: u32) -> Option<CatalogueEntry<'_>> {
        let mut lo = 0usize;
        let mut hi = self.count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let at = u32_at(self.bytes, HEAD_HEADER_LEN + mid * HEAD_RECORD_LEN)?;
            match at.cmp(&igdb_id) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return self.entry_at(mid),
            }
        }
        None
    }

    pub fn iter(&self) -> impl Iterator<Item = CatalogueEntry<'_>> {
        (0..self.count).filter_map(|i| self.entry_at(i))
    }
}

/// `steam_appid -> igdb_id` for every game IGDB knows about.
///
/// Held as an owned buffer rather than mmap'd: at ~1MB it is negligible
/// against the <50MB idle budget, and it avoids taking a dependency on a
/// memory-mapping crate for a file this small.
pub struct AppIdIndex {
    bytes: Vec<u8>,
    count: usize,
}

impl AppIdIndex {
    /// The compiled-in index. Every Steam game IGDB knows about resolves to an
    /// IGDB id offline, with no backend call.
    pub fn bundled() -> Option<Self> {
        Self::parse(INDEX_BYTES)
    }

    /// Parse the delta-encoded index into the flat, binary-searchable form.
    ///
    /// The shipped artifact stores `(delta_appid, igdb_id)` as varints — half
    /// the size of flat `(u32, u32)` pairs, which matters when it lives inside
    /// the installer. Expanding it costs one pass over 137k varints at startup
    /// and buys in-place binary search from then on.
    ///
    /// Returns `None` for anything malformed rather than answering with
    /// garbage game ids.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < INDEX_HEADER_LEN || &bytes[0..4] != MAGIC_INDEX {
            return None;
        }
        let count = u32_at(bytes, 4)? as usize;
        let mut flat = Vec::with_capacity(count * INDEX_PAIR_LEN);
        let mut pos = INDEX_HEADER_LEN;
        let mut appid: u32 = 0;

        for _ in 0..count {
            let (delta, next) = read_varint(bytes, pos)?;
            let (igdb_id, next) = read_varint(bytes, next)?;
            pos = next;
            appid = appid.checked_add(delta)?;
            flat.extend_from_slice(&appid.to_le_bytes());
            flat.extend_from_slice(&igdb_id.to_le_bytes());
        }
        Some(AppIdIndex { bytes: flat, count })
    }

    pub fn load(path: &std::path::Path) -> Option<Self> {
        Self::parse(&std::fs::read(path).ok()?)
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Resolve a Steam appid to its IGDB id. Pairs are sorted by appid at
    /// build time; binary search runs in place over the buffer.
    pub fn igdb_id(&self, appid: u32) -> Option<u32> {
        let mut lo = 0usize;
        let mut hi = self.count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            // No header in the decoded buffer: it holds pairs only.
            let o = mid * INDEX_PAIR_LEN;
            let at = u32_at(&self.bytes, o)?;
            match at.cmp(&appid) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return u32_at(&self.bytes, o + 4),
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head() -> Head {
        Head::bundled().expect("bundled head.bin must parse")
    }

    #[test]
    fn bundled_head_parses() {
        let h = head();
        assert!(h.len() > 1000, "expected a populated head, got {}", h.len());
        assert!(!h.is_empty());
    }

    #[test]
    fn resolves_known_games_by_igdb_id() {
        let h = head();
        let cs2 = h.get(242408).expect("Counter-Strike 2");
        assert_eq!(cs2.name, "Counter-Strike 2");
        assert_eq!(cs2.slug, "counter-strike-2");
        assert_eq!(cs2.short_name, "CS2");
        assert!(!cs2.cover_image.is_empty());

        assert_eq!(h.get(115).expect("League").short_name, "LoL");
        assert_eq!(h.get(2963).expect("Dota").name, "Dota 2");
    }

    #[test]
    fn head_covers_the_games_we_ship_telemetry_for() {
        // Every spec-18 adapter needs its game to resolve, or the post-game
        // card has telemetry with no name to attach it to.
        let h = head();
        for (igdb_id, label) in [
            (242408u32, "Counter-Strike 2"),
            (2963, "Dota 2"),
            (115, "League of Legends"),
            (11198, "Rocket League"),
            (1279, "Hearthstone"),
            (121, "Minecraft: Java Edition"),
        ] {
            let e = h.get(igdb_id).unwrap_or_else(|| panic!("{label} missing"));
            assert!(!e.name.is_empty(), "{label} has no name");
            assert!(!e.short_name.is_empty(), "{label} has no short name");
        }
    }

    #[test]
    fn curated_exes_resolve_regardless_of_launcher() {
        // The whole point of the curated table: these are top-50 games a Steam
        // library scan can never find, because they do not come from Steam.
        let h = head();
        for (exe, path, expect_game_id) in [
            (
                "Hearthstone.exe",
                r"C:\Program Files (x86)\Hearthstone\Hearthstone.exe",
                "hearthstone",
            ),
            (
                "League of Legends.exe",
                r"D:\Riot Games\League of Legends\Game\League of Legends.exe",
                "league-of-legends",
            ),
            (
                "VALORANT-Win64-Shipping.exe",
                r"C:\Riot Games\VALORANT\live\ShooterGame\Binaries\Win64\VALORANT-Win64-Shipping.exe",
                "valorant",
            ),
            (
                "FortniteClient-Win64-Shipping.exe",
                r"C:\Program Files\Epic Games\Fortnite\FortniteClient-Win64-Shipping.exe",
                "fortnite",
            ),
            (
                "RobloxPlayerBeta.exe",
                r"C:\Users\bob\AppData\Local\Roblox\Versions\RobloxPlayerBeta.exe",
                "roblox",
            ),
            (
                "Wow.exe",
                r"C:\Program Files (x86)\World of Warcraft\_retail_\Wow.exe",
                "world-of-warcraft",
            ),
        ] {
            let e = h
                .lookup_exe(exe, path)
                .unwrap_or_else(|| panic!("{exe} did not resolve"));
            assert_eq!(e.game_id, expect_game_id, "{exe}");
            assert!(e.igdb_id > 0, "{exe} has no igdb id");
        }
    }

    #[test]
    fn exe_matching_is_case_insensitive() {
        let h = head();
        let a = h.lookup_exe("cs2.exe", r"C:\x\cs2.exe").expect("lowercase");
        let b = h.lookup_exe("CS2.EXE", r"C:\x\CS2.EXE").expect("uppercase");
        assert_eq!(a.game_id, b.game_id);
        assert_eq!(a.game_id, "counter-strike-2");
    }

    #[test]
    fn shared_runtime_hosts_need_the_path_guard() {
        // javaw.exe is Minecraft only under a Minecraft install. Without the
        // guard, every Java desktop application reports as Minecraft — which
        // is exactly what the old hand-mapped catalogue did.
        let h = head();
        let mc = h
            .lookup_exe(
                "javaw.exe",
                r"C:\Users\bob\AppData\Roaming\.minecraft\runtime\javaw.exe",
            )
            .expect("Minecraft install should resolve");
        assert_eq!(mc.game_id, "minecraft");

        assert!(
            h.lookup_exe("javaw.exe", r"C:\Program Files\Eclipse\javaw.exe")
                .is_none(),
            "a Java IDE must not be reported as Minecraft"
        );
        assert!(
            h.lookup_exe("hl2.exe", r"C:\Steam\steamapps\common\Half-Life 2\hl2.exe")
                .is_none(),
            "hl2.exe outside Team Fortress must not resolve to TF2"
        );
    }

    #[test]
    fn game_id_stays_stable_where_igdb_disagrees() {
        // These ids key existing user_game_stats and the spec-18 adapters.
        // IGDB names both games differently; the curated override is what stops
        // v2 from orphaning stats already stored under the old ids.
        let h = head();
        let mc = h
            .lookup_exe("javaw.exe", r"C:\.minecraft\javaw.exe")
            .unwrap();
        assert_eq!(mc.game_id, "minecraft");
        assert_eq!(mc.slug, "minecraft-java-edition");

        let sc2 = h
            .lookup_exe("SC2_x64.exe", r"C:\StarCraft II\SC2_x64.exe")
            .unwrap();
        assert_eq!(sc2.game_id, "starcraft-2");
        assert!(sc2.slug.starts_with("starcraft-ii"));
    }

    #[test]
    fn every_telemetry_adapter_game_is_curated() {
        // A shipped adapter whose game the sensor cannot detect is dead code:
        // the config gets installed but no session ever opens to attach results
        // to. Five of these are non-Steam and exist only via the curated table.
        let h = head();
        let adapters = [
            ("counter-strike-2", "cs2.exe"),
            ("dota-2", "dota2.exe"),
            ("league-of-legends", "League of Legends.exe"),
            ("rocket-league", "RocketLeague.exe"),
            ("legends-of-runeterra", "LoR.exe"),
            ("hearthstone", "Hearthstone.exe"),
            ("minecraft", "javaw.exe"),
            ("path-of-exile", "PathOfExile.exe"),
            ("starcraft-2", "SC2_x64.exe"),
        ];
        for (game_id, exe) in adapters {
            let path = format!(r"C:\Games\{game_id}\{exe}");
            let e = h
                .lookup_exe(exe, &path)
                .unwrap_or_else(|| panic!("adapter game {game_id} has no curated exe mapping"));
            assert_eq!(e.game_id, game_id, "{exe} resolved to the wrong game");
        }
    }

    #[test]
    fn uncurated_exe_is_none() {
        let h = head();
        assert!(h
            .lookup_exe("notepad.exe", r"C:\Windows\notepad.exe")
            .is_none());
        assert!(h.lookup_exe("", "").is_none());
    }

    #[test]
    fn resolves_by_stable_game_id() {
        // Ledger events and stats rows carry game_id, not igdb_id, so every
        // surface that renders one needs this lookup.
        let h = head();
        let cs2 = h.by_game_id("counter-strike-2").expect("CS2");
        assert_eq!(cs2.short_name, "CS2");
        assert_eq!(cs2.igdb_id, 242408);

        // The curated override, not the IGDB slug.
        let mc = h.by_game_id("minecraft").expect("Minecraft");
        assert_eq!(mc.slug, "minecraft-java-edition");
    }

    #[test]
    fn discovered_game_ids_are_not_in_the_catalogue() {
        // steam-/epic-/local- ids are real sessions the catalogue has never
        // heard of. Callers must derive a badge rather than render a blank —
        // this returning None is the trigger for that fallback.
        let h = head();
        assert!(h.by_game_id("steam-1145360").is_none());
        assert!(h.by_game_id("local-night-stones").is_none());
        assert!(h.by_game_id("").is_none());
    }

    #[test]
    fn resolves_by_display_name_for_legacy_events() {
        let h = head();
        let e = h.by_name("Counter-Strike 2").expect("by name");
        assert_eq!(e.game_id, "counter-strike-2");
        assert_eq!(
            h.by_name("counter-strike 2").map(|e| e.game_id),
            Some("counter-strike-2"),
            "name matching must be case-insensitive"
        );
        assert!(h.by_name("").is_none());
    }

    #[test]
    fn unknown_id_is_none() {
        let h = head();
        assert!(h.get(0).is_none());
        assert!(h.get(u32::MAX).is_none());
    }

    #[test]
    fn head_records_are_sorted_and_well_formed() {
        // Binary search silently returns wrong answers on unsorted input, so
        // the ordering is an invariant of the artifact, not an assumption.
        let h = head();
        let mut prev = 0u32;
        for e in h.iter() {
            assert!(e.igdb_id > prev, "ids must strictly ascend");
            assert!(!e.name.is_empty(), "igdb {} has no name", e.igdb_id);
            prev = e.igdb_id;
        }
    }

    #[test]
    fn rejects_corrupt_artifacts() {
        assert!(Head::parse(b"").is_none());
        assert!(Head::parse(b"XXXX\0\0\0\0\0\0\0\0").is_none());
        // Valid magic, but the record table would run past the string blob.
        assert!(Head::parse(b"MHD1\xff\xff\xff\xff\x0c\x00\x00\x00").is_none());
    }

    // ---- appid index ----

    fn varint(mut n: u32) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (n & 0x7F) as u8;
            n >>= 7;
            if n == 0 {
                out.push(byte);
                return out;
            }
            out.push(byte | 0x80);
        }
    }

    /// Build a delta-encoded index the way the packer does.
    fn index_bytes(pairs: &[(u32, u32)]) -> Vec<u8> {
        let mut v = MAGIC_INDEX.to_vec();
        v.extend((pairs.len() as u32).to_le_bytes());
        let mut prev = 0u32;
        for (a, g) in pairs {
            v.extend(varint(a - prev));
            v.extend(varint(*g));
            prev = *a;
        }
        v
    }

    #[test]
    fn index_resolves_appids() {
        let idx = AppIdIndex::parse(&index_bytes(&[
            (570, 2963),
            (730, 242408),
            (1245620, 119133),
        ]))
        .expect("valid index");
        assert_eq!(idx.len(), 3);
        assert_eq!(idx.igdb_id(730), Some(242408));
        assert_eq!(idx.igdb_id(570), Some(2963));
        assert_eq!(idx.igdb_id(1245620), Some(119133));
    }

    #[test]
    fn index_misses_are_none() {
        let idx = AppIdIndex::parse(&index_bytes(&[(570, 2963), (730, 242408)])).unwrap();
        assert_eq!(idx.igdb_id(0), None);
        assert_eq!(idx.igdb_id(999), None);
        assert_eq!(idx.igdb_id(u32::MAX), None);
    }

    #[test]
    fn index_rejects_corrupt_input() {
        assert!(AppIdIndex::parse(&[]).is_none());
        assert!(AppIdIndex::parse(b"NOPE\0\0\0\0").is_none());
        // Header claims more pairs than the payload holds — a truncated file.
        // Values are large so each varint is several bytes; with tiny ones the
        // payload is only four bytes and there is nothing left to cut.
        let mut truncated = index_bytes(&[(1_000_000, 2_000_000), (3_000_000, 4_000_000)]);
        truncated.truncate(11);
        assert!(AppIdIndex::parse(&truncated).is_none());
    }

    #[test]
    fn empty_index_is_valid_but_answers_nothing() {
        let idx = AppIdIndex::parse(&index_bytes(&[])).expect("empty index is well-formed");
        assert!(idx.is_empty());
        assert_eq!(idx.igdb_id(730), None);
    }
}
