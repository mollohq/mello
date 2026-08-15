//! The game catalogue: IGDB identity and display metadata, derived from the
//! daily data dumps by `scripts/build_catalogue.py`.
//!
//! Two artifacts, split by how often they change and how they reach the client
//! (see `plans/GAME-SENSING-V2.md` §2.2):
//!
//! * **head** — the ~2,000 most-played games with full display metadata,
//!   compiled into the binary. Popular games resolve instantly and offline.
//! * **appid index** — `steam_appid -> igdb_id` for every game IGDB knows
//!   (~137k). Fetched at runtime and cached on disk, so catalogue freshness is
//!   decoupled from app releases.
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

const MAGIC_HEAD: &[u8; 4] = b"MHD1";
const MAGIC_INDEX: &[u8; 4] = b"MAI1";
/// igdb_id + four (offset, len) string refs.
const HEAD_RECORD_LEN: usize = 24;
const HEAD_HEADER_LEN: usize = 12;
/// (steam_appid u32, igdb_id u32)
const INDEX_PAIR_LEN: usize = 8;
const INDEX_HEADER_LEN: usize = 8;

/// A game's display metadata, borrowed straight out of the artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogueEntry<'a> {
    pub igdb_id: u32,
    pub name: &'a str,
    pub slug: &'a str,
    /// Badge-sized label, e.g. "CS2". Derived at build time with a curated
    /// override table.
    pub short_name: &'a str,
    /// IGDB cover `image_id`; empty when the game has no art. Combine with
    /// the image CDN to fetch. Not an icon — see §8.1.
    pub cover_image: &'a str,
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
        // Records must fit between the header and the string blob, and the
        // blob must start inside the file.
        let records_end = HEAD_HEADER_LEN.checked_add(count.checked_mul(HEAD_RECORD_LEN)?)?;
        if records_end > strings_off || strings_off > bytes.len() {
            return None;
        }
        Some(Head {
            bytes,
            count,
            strings_off,
        })
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
            name: self.string_at(g(4), rec[20]),
            slug: self.string_at(g(8), rec[21]),
            short_name: self.string_at(g(12), rec[22]),
            cover_image: self.string_at(g(16), rec[23]),
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
    /// Parse a fetched index. Returns `None` for anything that is not a
    /// well-formed index, so a truncated download is ignored rather than
    /// silently answering with garbage game ids.
    pub fn parse(bytes: Vec<u8>) -> Option<Self> {
        if bytes.len() < INDEX_HEADER_LEN || &bytes[0..4] != MAGIC_INDEX {
            return None;
        }
        let count = u32_at(&bytes, 4)? as usize;
        let needed = INDEX_HEADER_LEN.checked_add(count.checked_mul(INDEX_PAIR_LEN)?)?;
        if needed > bytes.len() {
            return None;
        }
        Some(AppIdIndex { bytes, count })
    }

    pub fn load(path: &std::path::Path) -> Option<Self> {
        Self::parse(std::fs::read(path).ok()?)
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
            let o = INDEX_HEADER_LEN + mid * INDEX_PAIR_LEN;
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

    fn index_bytes(pairs: &[(u32, u32)]) -> Vec<u8> {
        let mut v = MAGIC_INDEX.to_vec();
        v.extend((pairs.len() as u32).to_le_bytes());
        for (a, g) in pairs {
            v.extend(a.to_le_bytes());
            v.extend(g.to_le_bytes());
        }
        v
    }

    #[test]
    fn index_resolves_appids() {
        let idx = AppIdIndex::parse(index_bytes(&[
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
        let idx = AppIdIndex::parse(index_bytes(&[(570, 2963), (730, 242408)])).unwrap();
        assert_eq!(idx.igdb_id(0), None);
        assert_eq!(idx.igdb_id(999), None);
        assert_eq!(idx.igdb_id(u32::MAX), None);
    }

    #[test]
    fn index_rejects_corrupt_input() {
        assert!(AppIdIndex::parse(Vec::new()).is_none());
        assert!(AppIdIndex::parse(b"NOPE\0\0\0\0".to_vec()).is_none());
        // Header claims more pairs than the file holds — a truncated download.
        let mut truncated = index_bytes(&[(1, 1), (2, 2)]);
        truncated.truncate(12);
        assert!(AppIdIndex::parse(truncated).is_none());
    }

    #[test]
    fn empty_index_is_valid_but_answers_nothing() {
        let idx = AppIdIndex::parse(index_bytes(&[])).expect("empty index is well-formed");
        assert!(idx.is_empty());
        assert_eq!(idx.igdb_id(730), None);
    }
}
