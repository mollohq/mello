//! Bento grid ordering for the crew feed timeline.

use slint::Model;

use crate::FeedCardData;

const FILLER_SLOTS: usize = 7; // grid slots after hero + recap (3 + 1 + 1 wide + 2)
const WIDE_FILLER_INDEX: usize = 5; // index within the 7 filler cards → 2× wide grid cell

/// Quality score for session-preview cards. Higher = more deserving of hero / wide slots.
/// Short streams with only a couple of snapshots are heavily deprioritized.
pub fn session_preview_quality(card: &FeedCardData) -> i32 {
    if card.card_type.as_str() != "session-preview" {
        return i32::MIN / 2;
    }
    let snapshot_n = card.snapshot_urls.row_count() as i32;
    let dur = card.duration_min;

    // 1–2 min streams with ≤4 frames are not hero-worthy.
    if dur <= 2 && snapshot_n <= 4 {
        return -10_000 + snapshot_n;
    }

    let mut score = dur * 10 + snapshot_n * 3;
    if dur >= 15 {
        score += 40;
    }
    if snapshot_n >= 8 {
        score += 30;
    }
    score
}

fn card_type_str(card: &FeedCardData) -> &str {
    card.card_type.as_str()
}

/// Order timeline cards into the bento grid: hero, recap, then seven mixed filler slots.
pub fn order_feed_cards(cards: Vec<FeedCardData>) -> Vec<FeedCardData> {
    if cards.is_empty() {
        return cards;
    }

    let n = cards.len();
    let mut used = vec![false; n];
    let mut ordered: Vec<FeedCardData> = Vec::with_capacity(9);

    // Hero: best session-preview (visual priority over clips).
    if let Some(hi) = best_index(&cards, &used, "session-preview", session_preview_quality) {
        let mut hero = cards[hi].clone();
        hero.is_hero = true;
        ordered.push(hero);
        used[hi] = true;
    }

    // Recap pinned top-right.
    if let Some(ri) = first_index(&cards, &used, "recap") {
        ordered.push(cards[ri].clone());
        used[ri] = true;
    }

    let mut fillers = pick_filler_indices(&cards, &mut used);
    promote_wide_slot(&cards, &mut fillers);

    for idx in fillers {
        ordered.push(cards[idx].clone());
    }

    ordered
}

fn best_index(
    cards: &[FeedCardData],
    used: &[bool],
    card_type: &str,
    score: impl Fn(&FeedCardData) -> i32,
) -> Option<usize> {
    cards
        .iter()
        .enumerate()
        .filter(|(i, c)| !used[*i] && card_type_str(c) == card_type)
        .max_by_key(|(_, c)| score(c))
        .map(|(i, _)| i)
}

fn first_index(cards: &[FeedCardData], used: &[bool], card_type: &str) -> Option<usize> {
    cards
        .iter()
        .enumerate()
        .find(|(i, c)| !used[*i] && card_type_str(c) == card_type)
        .map(|(i, _)| i)
}

/// Pick seven filler indices with at least one of each present card type, then by priority.
fn pick_filler_indices(cards: &[FeedCardData], used: &mut [bool]) -> Vec<usize> {
    let mut picks: Vec<usize> = Vec::with_capacity(FILLER_SLOTS);

    // Diversity: one of each type that exists (timeline order = newest first within type).
    for card_type in ["clip", "session", "session-preview", "catchup"] {
        if picks.len() >= FILLER_SLOTS {
            break;
        }
        let idx = if card_type == "session-preview" {
            best_index(cards, used, "session-preview", session_preview_quality)
        } else {
            first_index(cards, used, card_type)
        };
        if let Some(i) = idx {
            picks.push(i);
            used[i] = true;
        }
    }

    // Priority fill: clips (living content), then strong previews, then other previews, sessions, catchups.
    let mut remaining: Vec<usize> = cards
        .iter()
        .enumerate()
        .filter(|(i, _)| !used[*i])
        .map(|(i, _)| i)
        .collect();

    remaining.sort_by(|&a, &b| filler_priority(&cards[b]).cmp(&filler_priority(&cards[a])));

    for i in remaining {
        if picks.len() >= FILLER_SLOTS {
            break;
        }
        picks.push(i);
        used[i] = true;
    }

    picks
}

fn filler_priority(card: &FeedCardData) -> i32 {
    match card_type_str(card) {
        "clip" => 10_000,
        "session-preview" => session_preview_quality(card),
        "session" => 100,
        "catchup" => 10,
        _ => 0,
    }
}

/// Put the best visual card (preview or clip) in the wide grid slot.
fn promote_wide_slot(cards: &[FeedCardData], picks: &mut [usize]) {
    if picks.len() <= WIDE_FILLER_INDEX {
        return;
    }

    let wide_pick = picks
        .iter()
        .enumerate()
        .filter(|(_, &i)| {
            let t = card_type_str(&cards[i]);
            t == "session-preview" || t == "clip"
        })
        .max_by_key(|(_, &i)| {
            let c = &cards[i];
            if card_type_str(c) == "session-preview" {
                session_preview_quality(c)
            } else {
                5_000 // clips beat weak previews for wide, lose to strong previews
            }
        })
        .map(|(pos, _)| pos);

    if let Some(from) = wide_pick {
        picks.swap(from, WIDE_FILLER_INDEX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::VecModel;
    use std::rc::Rc;

    fn preview(id: &str, duration_min: i32, snapshots: usize) -> FeedCardData {
        let urls: Vec<slint::SharedString> = (0..snapshots)
            .map(|i| format!("https://example.com/{id}/{i}.jpg").into())
            .collect();
        FeedCardData {
            id: id.into(),
            card_type: "session-preview".into(),
            duration_min,
            snapshot_urls: Rc::new(VecModel::from(urls)).into(),
            ..Default::default()
        }
    }

    fn typed(id: &str, card_type: &str) -> FeedCardData {
        FeedCardData {
            id: id.into(),
            card_type: card_type.into(),
            ..Default::default()
        }
    }

    #[test]
    fn weak_short_preview_scores_below_strong() {
        let weak = preview("short", 1, 2);
        let strong = preview("long", 21, 20);
        assert!(session_preview_quality(&strong) > session_preview_quality(&weak));
    }

    #[test]
    fn hero_prefers_strong_session_preview_over_clip() {
        let cards = vec![
            typed("recap", "recap"),
            typed("clip", "clip"),
            preview("short", 1, 2),
            typed("voice", "session"),
            preview("long", 21, 50),
        ];
        let ordered = order_feed_cards(cards);
        assert_eq!(ordered[0].card_type.as_str(), "session-preview");
        assert_eq!(ordered[0].id.as_str(), "long");
        assert!(ordered[0].is_hero);
    }

    #[test]
    fn includes_one_of_each_type_when_present() {
        let cards = vec![
            typed("recap", "recap"),
            typed("game", "session"),
            typed("voice", "session"),
            preview("p1", 1, 2),
            typed("clip", "clip"),
            preview("p2", 21, 30),
            typed("catch", "catchup"),
        ];
        let ordered = order_feed_cards(cards);
        let types: Vec<&str> = ordered.iter().map(|c| c.card_type.as_str()).collect();
        assert!(types.contains(&"recap"));
        assert!(types.contains(&"clip"));
        assert!(types.contains(&"session"));
        assert!(types.iter().filter(|t| **t == "session-preview").count() >= 2);
        assert!(types.contains(&"catchup"));
    }

    #[test]
    fn long_preview_survives_noise_sessions() {
        // Mirrors crew_timeline_resp: many game/voice sessions before the 21m preview.
        let cards = vec![
            typed("recap", "recap"),
            typed("g70", "session"),
            typed("v4", "session"),
            typed("v49", "session"),
            preview("short", 1, 2),
            preview("long", 21, 40),
        ];
        let ordered = order_feed_cards(cards);
        let preview_ids: Vec<&str> = ordered
            .iter()
            .filter(|c| c.card_type.as_str() == "session-preview")
            .map(|c| c.id.as_str())
            .collect();
        assert!(preview_ids.contains(&"long"));
        assert_eq!(ordered[0].id.as_str(), "long");
    }
}
