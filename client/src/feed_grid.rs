//! Where the cards of the this_week grid land.
//!
//! `crew_feed.slint` lays cards out by position, not by kind: the slot at a
//! fixed index is wide (two columns), whatever card sits there. Two rules need
//! the position, so they live here and mirror the layout in `crew_feed.slint`
//! (`row1-consumed`, `row2-base` and the row 3 wide slot):
//!
//! - A voice clip never takes a wide slot (CLIPS 7.1, plans/next-gen-feed.md
//!   section 2).
//! - A live session (a game in progress) takes the wide slot of the first row,
//!   and the head2head card sits beside it as the cell. When a hero leads the
//!   feed (a versus card, a hero clip), the live session takes the first cell
//!   of the side column. A live stream is above all of it.
//! - The recent games card is on the first screen for every crew size. In the
//!   plain layout (head2head cell and invite card lead) it takes the wide slot
//!   of the second row. Next to a hero or a wide head2head it takes the first
//!   cell of the side column.
//!
//! `arrange` is pure over card descriptors, so the rules are unit-tested
//! without a window. `arrange_model` applies it to the card model.

use slint::{Model, ModelRc, VecModel};
use std::rc::Rc;

use crate::FeedCardData;

/// The facts of one card that the layout rules read.
#[derive(Clone, Debug, Default)]
pub struct CardInfo {
    pub card_type: String,
    /// The head2head variant: "wide", "versus", "cell", "cell-full", "invite".
    pub variant: String,
    pub is_hero: bool,
    /// A recap with content takes the side column alone.
    pub recap_has_content: bool,
}

impl CardInfo {
    pub fn of(c: &FeedCardData) -> Self {
        CardInfo {
            card_type: c.card_type.to_string(),
            variant: c.head2head.variant.to_string(),
            is_hero: c.is_hero,
            recap_has_content: c.card_type == "recap"
                && (c.leaderboard.row_count() > 0
                    || c.awards.row_count() > 0
                    || c.belts_moved.row_count() > 0
                    || c.belts_held.row_count() > 0),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Lead {
    /// Head2head wide: two columns of the first row, one card beside it.
    Wide,
    /// A hero clip or a versus card: one full row.
    FullRow,
    /// A head2head cell or the invite card: a plain one-column card.
    Plain,
    /// A hero (two columns, two rows) with a side column.
    Hero,
}

struct Layout {
    lead: Lead,
    start: usize,
    row2_base: usize,
}

fn layout(cards: &[CardInfo], start: usize) -> Layout {
    let at = |i: usize| cards.get(i);
    let first = at(start);
    let variant = first
        .filter(|c| c.card_type == "head2head")
        .map(|c| c.variant.as_str())
        .unwrap_or("");
    let lead = match first {
        None => Lead::Hero,
        Some(c) if c.card_type == "live-session" => Lead::Wide,
        Some(c) if c.card_type == "clip" && c.is_hero => Lead::FullRow,
        Some(_) if variant == "versus" => Lead::FullRow,
        Some(_) if variant == "wide" => Lead::Wide,
        Some(c) if variant == "cell" || variant == "cell-full" || c.card_type == "invite" => {
            Lead::Plain
        }
        Some(_) => Lead::Hero,
    };
    let side_recap = at(start + 1).is_some_and(|c| c.card_type == "recap" && c.recap_has_content);
    let left = cards.len().saturating_sub(start);
    let consumed = if cards.len() <= start {
        0
    } else {
        match lead {
            Lead::Plain => 0,
            Lead::Wide => left.min(2),
            Lead::FullRow => 1,
            Lead::Hero if side_recap => left.min(2),
            Lead::Hero => left.min(3),
        }
    };
    Layout {
        lead,
        start,
        row2_base: start + consumed,
    }
}

/// The indices of the two-column slots for `len` cards.
fn wide_slots(l: &Layout, len: usize) -> Vec<usize> {
    let mut w = Vec::new();
    if l.lead == Lead::Hero && l.start < len {
        w.push(l.start); // the hero, two columns and two rows
    }
    if l.lead == Lead::Wide && l.start < len {
        w.push(l.start);
    }
    if l.row2_base + 4 < len {
        w.push(l.row2_base + 4); // row 3: one card and a wide card
    }
    w
}

/// The indices of the two-column slots of the cards, in the order on screen.
#[cfg(test)]
pub fn wide_slot_indices(cards: &[CardInfo], start: usize) -> Vec<usize> {
    wide_slots(&layout(cards, start), cards.len())
}

/// Cards that keep their place: they decide the layout or are a fixed item of
/// it. Everything else may swap with a clip.
fn is_fixed(t: &str) -> bool {
    matches!(
        t,
        "clip" | "head2head" | "invite" | "recap" | "recent-games" | "locked" | "live-session"
    )
}

/// The new order, as indices into `cards`.
///
/// `start` is the first card of the grid: the live stream beside the feed takes
/// two cards when exactly one stream is live (`stream-side-consumed`).
pub fn arrange(cards: &[CardInfo], start: usize) -> Vec<usize> {
    let lives: Vec<usize> = (0..cards.len())
        .filter(|&i| cards[i].card_type == "live-session")
        .collect();
    let Some(&live) = lives.first() else {
        return arrange_core(cards, start);
    };
    // The first game leads. More games are plain cells behind the first row.
    let more = &lives[1..];
    let place_more = |mut out: Vec<usize>| {
        for (n, &i) in more.iter().enumerate() {
            out.insert((start + 2 + n).min(out.len()), i);
        }
        out
    };
    let rest: Vec<usize> = (0..cards.len()).filter(|i| !lives.contains(i)).collect();
    let hero = rest.get(start).is_some_and(|&i| {
        cards[i].is_hero || (cards[i].card_type == "head2head" && cards[i].variant == "versus")
    });
    if hero {
        // The hero keeps its place. The live session is the first side cell.
        let infos: Vec<CardInfo> = rest.iter().map(|&i| cards[i].clone()).collect();
        let mut out: Vec<usize> = arrange_core(&infos, start)
            .into_iter()
            .map(|j| rest[j])
            .collect();
        out.insert((start + 1).min(out.len()), live);
        return place_more(out);
    }
    // The live session leads and the head2head card is the cell beside it.
    let mut seq = rest;
    seq.insert(start.min(seq.len()), live);
    if let Some(p) = seq
        .iter()
        .position(|&i| cards[i].card_type == "head2head")
        .filter(|&p| p > start + 1)
    {
        let id = seq.remove(p);
        seq.insert(start + 1, id);
    }
    let infos: Vec<CardInfo> = seq.iter().map(|&i| cards[i].clone()).collect();
    place_more(
        arrange_core(&infos, start)
            .into_iter()
            .map(|j| seq[j])
            .collect(),
    )
}

fn arrange_core(cards: &[CardInfo], start: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..cards.len()).collect();
    if cards.len() <= start {
        return order;
    }

    // 1. Recent games to the first screen.
    if let Some(from) = order
        .iter()
        .position(|&i| cards[i].card_type == "recent-games")
    {
        let infos = |o: &[usize]| o.iter().map(|&i| cards[i].clone()).collect::<Vec<_>>();
        let l = layout(&infos(&order), start);
        let target = match l.lead {
            // The wide slot of the second row, with the head2head cell and
            // invite card in the first.
            Lead::Plain => l.row2_base + 4,
            // Next to the lead, in the side column.
            // Beside a live session, behind the head2head cell that sits there.
            Lead::Wide
                if infos(&order)
                    .get(start)
                    .is_some_and(|c| c.card_type == "live-session")
                    && infos(&order)
                        .get(start + 1)
                        .is_some_and(|c| c.card_type == "head2head") =>
            {
                start + 2
            }
            Lead::Wide | Lead::Hero | Lead::FullRow => start + 1,
        };
        let target = target.min(order.len() - 1);
        if from != target {
            let id = order.remove(from);
            order.insert(target, id);
        }
    }

    // 2. A clip leaves every wide slot. Swap it with the nearest card that is
    // not fixed, later cards first. With none, the clip stays.
    let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
    let l = layout(&infos, start);
    for w in wide_slots(&l, order.len()) {
        if infos[w].card_type != "clip" {
            continue;
        }
        let wide = wide_slots(&l, order.len());
        let swap = (1..order.len()).find_map(|step| {
            [w + step, w.wrapping_sub(step)].into_iter().find(|&p| {
                p < order.len()
                    && p >= start
                    && !wide.contains(&p)
                    && !is_fixed(&cards[order[p]].card_type)
            })
        });
        if let Some(p) = swap {
            order.swap(w, p);
        }
    }
    order
}

/// Apply `arrange` to a card list.
pub fn arrange_cards(cards: Vec<FeedCardData>, start: usize) -> Vec<FeedCardData> {
    let infos: Vec<CardInfo> = cards.iter().map(CardInfo::of).collect();
    let order = arrange(&infos, start);
    let mut slots: Vec<Option<FeedCardData>> = cards.into_iter().map(Some).collect();
    order.into_iter().filter_map(|i| slots[i].take()).collect()
}

/// The first grid card: one live stream takes the two first cards of the feed
/// into its side column (`stream-side-consumed` in `crew_feed.slint`).
pub fn grid_start(live_streams: usize, cards: usize) -> usize {
    if live_streams == 1 {
        cards.min(2)
    } else {
        0
    }
}

/// Arrange the cards on screen again, for a change that does not rebuild them
/// (a new live stream, a clip recorded here).
pub fn arrange_model(model: &ModelRc<FeedCardData>, live_streams: usize) -> ModelRc<FeedCardData> {
    let cards: Vec<FeedCardData> = (0..model.row_count())
        .filter_map(|i| model.row_data(i))
        .collect();
    let start = grid_start(live_streams, cards.len());
    Rc::new(VecModel::from(arrange_cards(cards, start))).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(t: &str) -> CardInfo {
        CardInfo {
            card_type: t.into(),
            ..Default::default()
        }
    }
    fn h2h(variant: &str) -> CardInfo {
        CardInfo {
            card_type: "head2head".into(),
            variant: variant.into(),
            ..Default::default()
        }
    }
    fn types(cards: &[CardInfo], order: &[usize]) -> Vec<String> {
        order.iter().map(|&i| cards[i].card_type.clone()).collect()
    }

    /// The fifty-member feed of the e2e stage, with the invite card the client adds.
    fn fifty() -> Vec<CardInfo> {
        vec![
            h2h("cell"),
            card("invite"),
            card("clip"),
            card("game-session"),
            card("clip"),
            card("clip"),
            card("clip"),
            card("recent-games"),
            card("clip"),
        ]
    }

    #[test]
    fn recent_games_takes_the_wide_slot_of_the_second_row() {
        let cards = fifty();
        let order = arrange(&cards, 0);
        let t = types(&cards, &order);
        assert_eq!(t[4], "recent-games", "{t:?}");
    }

    #[test]
    fn no_clip_is_in_a_wide_slot() {
        let cards = fifty();
        let order = arrange(&cards, 0);
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        let l = layout(&infos, 0);
        for w in wide_slots(&l, infos.len()) {
            assert_ne!(infos[w].card_type, "clip", "slot {w}");
        }
    }

    #[test]
    fn five_clips_and_nothing_else_keep_the_clip_where_it_is() {
        // Nothing to swap with: a clip stays rather than a fixed card moving.
        let cards = vec![
            h2h("cell"),
            card("invite"),
            card("clip"),
            card("clip"),
            card("clip"),
            card("clip"),
            card("clip"),
        ];
        let order = arrange(&cards, 0);
        assert_eq!(order.len(), cards.len());
    }

    #[test]
    fn recent_games_sits_beside_a_wide_head2head() {
        let cards = vec![
            h2h("wide"),
            card("clip"),
            card("game-session"),
            card("recent-games"),
        ];
        let order = arrange(&cards, 0);
        assert_eq!(types(&cards, &order)[1], "recent-games");
    }

    #[test]
    fn a_card_order_without_recent_games_is_kept_but_clips_leave_wide() {
        let cards = vec![
            h2h("cell"),
            card("invite"),
            card("clip"),
            card("clip"),
            card("clip"),
            card("game-session"),
        ];
        let order = arrange(&cards, 0);
        assert_eq!(types(&cards, &order)[4], "game-session");
    }

    #[test]
    fn a_live_stream_moves_the_start() {
        assert_eq!(grid_start(1, 5), 2);
        assert_eq!(grid_start(0, 5), 0);
        assert_eq!(grid_start(2, 5), 0);
    }

    fn live() -> CardInfo {
        card("live-session")
    }

    #[test]
    fn a_live_session_takes_the_wide_slot_and_the_head2head_cell_sits_beside_it() {
        let cards = vec![
            h2h("wide"),
            card("recent-games"),
            card("game-session"),
            live(),
        ];
        let order = arrange(&cards, 0);
        let t = types(&cards, &order);
        assert_eq!(t[0], "live-session", "{t:?}");
        assert_eq!(t[1], "head2head", "{t:?}");
        assert_eq!(t[2], "recent-games", "{t:?}");
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        assert_eq!(wide_slots(&layout(&infos, 0), infos.len())[0], 0);
    }

    #[test]
    fn a_live_session_leads_a_crew_whose_cell_leads() {
        let cards = vec![h2h("cell"), card("invite"), card("game-session"), live()];
        let t = types(&cards, &arrange(&cards, 0));
        assert_eq!(&t[..3], ["live-session", "head2head", "invite"], "{t:?}");
    }

    #[test]
    fn a_hero_keeps_its_place_and_the_live_session_is_the_first_side_cell() {
        let mut hero = card("recap");
        hero.is_hero = true;
        let cards = vec![hero, card("recent-games"), card("game-session"), live()];
        let t = types(&cards, &arrange(&cards, 0));
        assert_eq!(&t[..3], ["recap", "live-session", "recent-games"], "{t:?}");
        let cards = vec![h2h("versus"), card("game-session"), live()];
        let t = types(&cards, &arrange(&cards, 0));
        assert_eq!(&t[..2], ["head2head", "live-session"], "{t:?}");
    }

    #[test]
    fn a_live_stream_keeps_the_side_column_and_the_live_session_leads_the_grid() {
        let cards = vec![card("game-session"), card("clip"), h2h("wide"), live()];
        let t = types(&cards, &arrange(&cards, 2));
        assert_eq!(t[2], "live-session", "{t:?}");
    }

    #[test]
    fn a_second_game_is_a_cell_behind_the_first_row() {
        let cards = vec![
            h2h("cell"),
            card("recent-games"),
            card("game-session"),
            live(),
            live(),
        ];
        let order = arrange(&cards, 0);
        let t = types(&cards, &order);
        assert_eq!(
            &t[..4],
            ["live-session", "head2head", "live-session", "recent-games"],
            "{t:?}"
        );
        let again: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        assert_eq!(arrange(&again, 0), (0..cards.len()).collect::<Vec<_>>());
    }

    #[test]
    fn arranging_twice_gives_the_same_order() {
        let cards = vec![
            h2h("wide"),
            card("recent-games"),
            card("game-session"),
            live(),
        ];
        let once = arrange(&cards, 0);
        let infos: Vec<CardInfo> = once.iter().map(|&i| cards[i].clone()).collect();
        let twice = arrange(&infos, 0);
        assert_eq!(twice, (0..cards.len()).collect::<Vec<_>>());
    }
}
