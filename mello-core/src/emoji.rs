//! The picker's emoji: the full Unicode set the bundled font covers, with
//! names for search and accessibility. The table is generated, see
//! `tools/emoji/gen_emoji_table.py`.

pub use crate::emoji_table::CATEGORIES;
use crate::emoji_table::EMOJI;

pub struct EmojiEntry {
    pub emoji: &'static str,
    pub name: &'static str,
    /// Index into [`CATEGORIES`].
    pub category: usize,
}

fn entry(row: &(&'static str, &'static str, u8)) -> EmojiEntry {
    EmojiEntry {
        emoji: row.0,
        name: row.1,
        category: row.2 as usize,
    }
}

/// Every emoji, in Unicode order.
pub fn all_emoji() -> Vec<EmojiEntry> {
    EMOJI.iter().map(entry).collect()
}

/// The emoji of one category, in Unicode order.
pub fn emoji_in_category(category: usize) -> Vec<EmojiEntry> {
    EMOJI
        .iter()
        .filter(|(_, _, c)| *c as usize == category)
        .map(entry)
        .collect()
}

/// Emoji whose name holds every word of `query`. The exact name comes first,
/// then names that start with the query: "fire" finds 🔥 before 🧑‍🚒.
pub fn search_emoji(query: &str) -> Vec<EmojiEntry> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let words: Vec<&str> = q.split_whitespace().collect();
    // Rank: the exact name, then a name that starts with the query, then
    // a name that holds every word. Stable, so Unicode order breaks ties.
    let mut hits: Vec<(u8, EmojiEntry)> = EMOJI
        .iter()
        .filter_map(|row| {
            let name = row.1.to_lowercase();
            if !words.iter().all(|w| name.contains(w)) {
                return None;
            }
            let rank = if name == q {
                0
            } else if name.starts_with(&q) {
                1
            } else {
                2
            };
            Some((rank, entry(row)))
        })
        .collect();
    hits.sort_by_key(|(rank, _)| *rank);
    hits.into_iter().map(|(_, e)| e).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_the_full_set_in_eight_categories() {
        assert!(all_emoji().len() > 1500, "{}", all_emoji().len());
        assert_eq!(CATEGORIES.len(), 8);
        for c in 0..CATEGORIES.len() {
            assert!(!emoji_in_category(c).is_empty(), "category {c} has emoji");
        }
        assert!(all_emoji().iter().all(|e| e.category < CATEGORIES.len()));
    }

    #[test]
    fn search_finds_by_name_and_puts_the_exact_start_first() {
        let fire = search_emoji("fire");
        assert_eq!(fire[0].emoji, "🔥");
        assert!(
            fire.iter().any(|e| e.emoji == "🧯"),
            "fire extinguisher is a hit too"
        );
        assert_eq!(search_emoji("  ").len(), 0);
        assert_eq!(search_emoji("thumbs up")[0].emoji, "👍");
        assert_eq!(search_emoji("flag sweden")[0].emoji, "🇸🇪");
    }
}
