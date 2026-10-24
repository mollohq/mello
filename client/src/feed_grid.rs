//! Where the cards of the this_week grid land.
//!
//! `crew_feed.slint` lays cards out by position, not by kind: the slot at a
//! fixed index is wide (two columns), whatever card sits there. Two rules need
//! the position, so they live here and mirror the layout in `crew_feed.slint`
//! (`row1-consumed`, `row2-base` and the row 3 wide slot):
//!
//! - A session of a crew of one stands left of the wide head2head card
//!   (frame B): one cell, then two columns. `Lead::CellWide`.
//! - A voice clip never takes a wide slot (CLIPS 7.1, plans/next-gen-feed.md
//!   section 2).
//! - A live session (a game in progress) with poster art takes the wide slot
//!   of the first row, and the head2head card sits beside it as the cell. When
//!   a hero leads the feed (a versus card, a hero clip), the live session takes
//!   the first cell of the side column. A live stream is above all of it.
//! - A live session without poster art is a plain cell. It never takes the
//!   wide slot: the slot goes to the next card in the hero order (the wide
//!   head2head card of a crew of one, else the card the grid leads with). The
//!   cell sits beside the lead, in the first cell of the side column.
//! - The recent games card is a poster grid when two or more members own
//!   games (`recent_pc_count`). The grid takes two columns of a row. When it
//!   leads row 2 (`r2_wide`), that row holds the grid and one card, and every
//!   row after it moves up by one card.
//! - Under a hero the poster grid takes the wide slot of row 3 (frames C and
//!   D). The one-PC list stays in the side column.
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
    /// A live session with poster art: the only live card for the wide slot.
    pub has_poster: bool,
    /// A recap with content takes the side column alone.
    pub recap_has_content: bool,
    /// The recent games card as a poster grid: two or more members own games.
    pub recent_grid: bool,
}

impl CardInfo {
    pub fn of(c: &FeedCardData) -> Self {
        CardInfo {
            card_type: c.card_type.to_string(),
            variant: c.head2head.variant.to_string(),
            is_hero: c.is_hero,
            has_poster: c.live.has_poster,
            recent_grid: c.card_type == "recent-games" && c.recent_pc_count >= 2,
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
    /// A game session cell, then the wide head2head card of a crew of one.
    CellWide,
    /// The belt hero of a quiet feed (frame E): two columns and one row, the
    /// head2head cell tall beside it, two cells under it. Four cards.
    Belt,
    /// Two stacked cells, the head2head cell tall and a recap tall (frame F).
    /// Four cards.
    TallTrio,
}

struct Layout {
    lead: Lead,
    start: usize,
    row2_base: usize,
    /// The poster grid leads row 2: two columns, then one card.
    r2_wide: bool,
}

impl Layout {
    /// The first card of row 3: row 2 holds two cards with the grid, else three.
    fn row3_base(&self) -> usize {
        self.row2_base + if self.r2_wide { 2 } else { 3 }
    }
}

/// Frame F: a head2head cell and a recap with content stand in the third and
/// fourth place, so they are the two tall cards beside two stacked cells.
fn tall_trio(cards: &[CardInfo], start: usize) -> bool {
    let h2h = cards.get(start + 2).is_some_and(|c| {
        c.card_type == "head2head" && (c.variant == "cell" || c.variant == "cell-full")
    });
    let recap = cards
        .get(start + 3)
        .is_some_and(|c| c.card_type == "recap" && c.recap_has_content);
    h2h && recap
}

fn layout(cards: &[CardInfo], start: usize) -> Layout {
    let at = |i: usize| cards.get(i);
    let first = at(start);
    let variant = first
        .filter(|c| c.card_type == "head2head")
        .map(|c| c.variant.as_str())
        .unwrap_or("");
    let session_then_wide = first.is_some_and(|c| c.card_type == "game-session")
        && at(start + 1).is_some_and(|c| c.card_type == "head2head" && c.variant == "wide");
    let lead = match first {
        None => Lead::Hero,
        Some(_) if session_then_wide => Lead::CellWide,
        Some(_) if variant == "belt" => Lead::Belt,
        Some(c) if !c.is_hero && tall_trio(cards, start) => Lead::TallTrio,
        Some(c) if c.card_type == "live-session" && c.has_poster => Lead::Wide,
        Some(c) if c.card_type == "clip" && c.is_hero => Lead::FullRow,
        Some(_) if variant == "versus" => Lead::FullRow,
        Some(_) if variant == "wide" => Lead::Wide,
        Some(c) if variant == "cell" || variant == "cell-full" || c.card_type == "invite" => {
            Lead::Plain
        }
        // After a live stream the grid is plain rows: a card there is a cell.
        Some(c) if start > 0 && !c.is_hero => Lead::Plain,
        Some(_) => Lead::Hero,
    };
    let side_recap = at(start + 1).is_some_and(|c| c.card_type == "recap" && c.recap_has_content);
    let left = cards.len().saturating_sub(start);
    let consumed = if cards.len() <= start {
        0
    } else {
        match lead {
            Lead::Plain => 0,
            Lead::Wide | Lead::CellWide => left.min(2),
            Lead::Belt | Lead::TallTrio => left.min(4),
            Lead::FullRow => 1,
            Lead::Hero if side_recap => left.min(2),
            Lead::Hero => left.min(3),
        }
    };
    let row2_base = start + consumed;
    Layout {
        lead,
        start,
        row2_base,
        r2_wide: at(row2_base).is_some_and(|c| c.recent_grid),
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
    if l.lead == Lead::CellWide && l.start + 1 < len {
        w.push(l.start + 1); // the session cell, then the wide card
    }
    if l.r2_wide {
        w.push(l.row2_base); // row 2: the poster grid and one card
    }
    if l.row3_base() + 1 < len {
        w.push(l.row3_base() + 1); // row 3: one card and a wide card
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
    if lives.is_empty() {
        return arrange_core(cards, start, true);
    }
    let rest: Vec<usize> = (0..cards.len()).filter(|i| !lives.contains(i)).collect();
    // The first game with poster art leads. Every other game is a plain cell.
    let Some(&live) = lives.iter().find(|&&i| cards[i].has_poster) else {
        return arrange_cells(cards, start, &rest, &lives);
    };
    let more: Vec<usize> = lives.iter().copied().filter(|&i| i != live).collect();
    let more = &more[..];
    let place_more = |mut out: Vec<usize>| {
        for (n, &i) in more.iter().enumerate() {
            out.insert((start + 2 + n).min(out.len()), i);
        }
        clips_leave_wide(cards, &mut out, start);
        out
    };
    let hero = rest.get(start).is_some_and(|&i| {
        cards[i].is_hero || (cards[i].card_type == "head2head" && cards[i].variant == "versus")
    });
    if hero {
        // The hero keeps its place. The live session is the first side cell.
        let infos: Vec<CardInfo> = rest.iter().map(|&i| cards[i].clone()).collect();
        let mut out: Vec<usize> = arrange_core(&infos, start, false)
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
        arrange_core(&infos, start, false)
            .into_iter()
            .map(|j| seq[j])
            .collect(),
    )
}

/// No live session has poster art: they are all cells. The cards without them
/// take the layout, so the wide slot goes to the next card in the hero order.
/// The first cell sits beside the lead (behind a recap that holds the side
/// column), the others follow it.
fn arrange_cells(cards: &[CardInfo], start: usize, rest: &[usize], lives: &[usize]) -> Vec<usize> {
    let infos: Vec<CardInfo> = rest.iter().map(|&i| cards[i].clone()).collect();
    let mut out: Vec<usize> = arrange_core(&infos, start, false)
        .into_iter()
        .map(|j| rest[j])
        .collect();
    let side_recap = out
        .get(start + 1)
        .is_some_and(|&i| cards[i].card_type == "recap" && cards[i].recap_has_content);
    let base = start + 1 + usize::from(side_recap);
    for (n, &i) in lives.iter().enumerate() {
        out.insert((base + n).min(out.len()), i);
    }
    clips_leave_wide(cards, &mut out, start);
    out
}

/// `no_live`: no live session is in the feed. The first session of a crew of
/// one then stands left of the wide head2head card, and a quiet feed gets its
/// tall cards. Off with a live session: its cell sits there.
fn arrange_core(cards: &[CardInfo], start: usize, no_live: bool) -> Vec<usize> {
    let mut order: Vec<usize> = (0..cards.len()).collect();
    if cards.len() <= start {
        return order;
    }

    // 0. A session of a crew of one leads the wide head2head card (frame B).
    if no_live
        && cards
            .get(start)
            .is_some_and(|c| c.card_type == "head2head" && c.variant == "wide")
    {
        if let Some(p) = order
            .iter()
            .position(|&i| cards[i].card_type == "game-session")
        {
            let id = order.remove(p);
            order.insert(start, id);
        }
    }

    // 0b. A feed with no hero, a head2head cell and a recap with content: the
    // two tall cards go third and fourth, two plain cells stack beside them
    // (frame F, a crew that was away).
    if no_live {
        tall_trio_order(cards, &mut order, start);
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
            // The session cell and the wide card fill row 1. The list is the
            // first cell of row 2, with the skeletons beside it.
            Lead::CellWide => start + 2,
            // The third cell of row 2, beside the two sessions (frame E).
            Lead::Belt | Lead::TallTrio => l.row2_base + 2,
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
            // The poster grid needs two columns. Under a hero it takes the wide
            // slot of row 3 (frames C and D). The list stays in the side column.
            Lead::Hero if cards[from].recent_grid => l.row2_base + 4,
            Lead::Wide | Lead::Hero | Lead::FullRow => start + 1,
        };
        let target = target.min(order.len() - 1);
        if from != target {
            let id = order.remove(from);
            order.insert(target, id);
        }
    }

    clips_leave_wide(cards, &mut order, start);
    order
}

/// Put a head2head cell and a recap with content in the third and fourth
/// place, after two plain cells, when the feed has no hero (frame F). A feed
/// that already has a lead of its own keeps it.
fn tall_trio_order(cards: &[CardInfo], order: &mut Vec<usize>, start: usize) {
    let at = |o: &[usize], p: usize| o.get(p).map(|&i| &cards[i]);
    let has_lead = at(order, start).is_none_or(|c| {
        c.is_hero
            || matches!(
                c.card_type.as_str(),
                "live-session" | "invite" | "recent-games"
            )
            || (c.card_type == "head2head" && c.variant != "cell" && c.variant != "cell-full")
    });
    if has_lead {
        return;
    }
    let find = |o: &[usize], f: &dyn Fn(&CardInfo) -> bool| o.iter().position(|&i| f(&cards[i]));
    let (Some(h), Some(r)) = (
        find(order, &|c| {
            c.card_type == "head2head" && (c.variant == "cell" || c.variant == "cell-full")
        }),
        find(order, &|c| c.card_type == "recap" && c.recap_has_content),
    ) else {
        return;
    };
    // Two plain cells to stack: anything that is not a fixed card.
    let plain: Vec<usize> = order
        .iter()
        .copied()
        .filter(|&i| !is_fixed(&cards[i].card_type))
        .take(2)
        .collect();
    if plain.len() < 2 {
        return;
    }
    let (h_id, r_id) = (order[h], order[r]);
    order.retain(|i| !plain.contains(i) && *i != h_id && *i != r_id);
    let mut head = plain;
    head.push(h_id);
    head.push(r_id);
    for (n, i) in head.into_iter().enumerate() {
        order.insert((start + n).min(order.len()), i);
    }
}

/// A clip leaves every wide slot. Swap it with the nearest card that is not
/// fixed, later cards first. With none, the clip stays.
fn clips_leave_wide(cards: &[CardInfo], order: &mut [usize], start: usize) {
    if order.len() <= start {
        return;
    }
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

/// The first grid card beside a two-stream hero (frame D): a recap with
/// content holds the whole side column, so it is the one card there. Else the
/// first two cards stand in it. Mirrors `stream-side-consumed` in
/// `crew_feed.slint`.
///
/// The client holds one remote streamer today, so no caller passes two streams
/// yet. The layout is drawn from mock data (`PreviewFeedFiveMembers`) and this
/// rule is tested here, for the phase that adds the second stream.
#[cfg_attr(not(test), allow(dead_code))]
pub fn two_stream_start(cards: &[CardInfo]) -> usize {
    match cards.first() {
        None => 0,
        Some(c) if c.card_type == "recap" && c.recap_has_content => 1,
        Some(_) => cards.len().min(2),
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
        // The session leads the wide card (frame B), so the list opens row 2.
        assert_eq!(
            &types(&cards, &order)[..3],
            ["game-session", "head2head", "recent-games"]
        );
    }

    #[test]
    fn a_session_stands_left_of_the_wide_head2head_card() {
        // Frame B: the card of the first session, then the wide card.
        let cards = vec![h2h("wide"), card("recent-games"), card("game-session")];
        let order = arrange(&cards, 0);
        let t = types(&cards, &order);
        assert_eq!(t, ["game-session", "head2head", "recent-games"], "{t:?}");
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        let l = layout(&infos, 0);
        assert_eq!(l.lead, Lead::CellWide);
        assert_eq!(
            wide_slots(&l, infos.len()),
            [1],
            "the wide card is the second"
        );
        assert_eq!(l.row2_base, 2, "row 1 holds the session and the wide card");
        // Arranging again changes nothing.
        assert_eq!(arrange(&infos, 0), (0..infos.len()).collect::<Vec<_>>());
    }

    #[test]
    fn a_crew_of_one_with_no_session_keeps_the_wide_card_first() {
        // Frame A: no session yet.
        let cards = vec![h2h("wide"), card("recent-games")];
        let t = types(&cards, &arrange(&cards, 0));
        assert_eq!(t, ["head2head", "recent-games"], "{t:?}");
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

    fn grid() -> CardInfo {
        CardInfo {
            card_type: "recent-games".into(),
            recent_grid: true,
            ..Default::default()
        }
    }

    #[test]
    fn the_poster_grid_leads_row_two_under_a_versus_card() {
        // Frame B2: the versus hero, then the grid and the session card.
        let cards = vec![h2h("versus"), card("game-session"), grid()];
        let order = arrange(&cards, 0);
        let t = types(&cards, &order);
        assert_eq!(t, ["head2head", "recent-games", "game-session"]);
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        let l = layout(&infos, 0);
        assert!(l.r2_wide);
        assert_eq!(wide_slots(&l, infos.len()), [1], "the grid is wide");
        assert_eq!(l.row3_base(), 3, "row 2 holds two cards");
    }

    #[test]
    fn the_list_stays_one_column_when_one_pc_owns_the_games() {
        let cards = vec![h2h("versus"), card("game-session"), card("recent-games")];
        let order = arrange(&cards, 0);
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        let l = layout(&infos, 0);
        assert!(!l.r2_wide);
        assert!(wide_slots(&l, infos.len()).is_empty());
        assert_eq!(l.row3_base(), 4, "row 2 holds three cards");
    }

    #[test]
    fn rows_after_the_poster_grid_move_up_by_one_card() {
        let cards = vec![
            h2h("versus"),
            grid(),
            card("game-session"),
            card("session"),
            card("daily-recap"),
            card("session"),
        ];
        let infos = cards.clone();
        let l = layout(&infos, 0);
        // Row 2: the grid and the session card. Row 3: card 3 and the wide card 4.
        assert_eq!(wide_slots(&l, infos.len()), [1, 4]);
    }

    #[test]
    fn a_crew_of_one_keeps_the_list_beside_the_wide_head2head() {
        // Frame A: one PC, so the list sits in the column beside the wide card.
        let cards = vec![h2h("wide"), card("recent-games")];
        let order = arrange(&cards, 0);
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        assert!(!layout(&infos, 0).r2_wide);
    }

    #[test]
    fn the_poster_grid_takes_the_wide_slot_of_row_three_under_a_hero() {
        // Frame C: the stream snapshot hero, two side cards, three cells in
        // row 2, then a clip and the grid.
        let mut hero = card("session-preview");
        hero.is_hero = true;
        let cards = vec![
            hero,
            h2h("cell"),
            card("session"),
            card("game-session"),
            card("game-session"),
            card("daily-recap"),
            card("clip"),
            grid(),
        ];
        let order = arrange(&cards, 0);
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        let l = layout(&infos, 0);
        let wide = wide_slots(&l, infos.len());
        assert_eq!(wide, [0, 7], "the hero and the grid");
        assert_eq!(infos[7].card_type, "recent-games");
        assert_eq!(infos[6].card_type, "clip", "a clip is a cell, never wide");
    }

    #[test]
    fn the_one_pc_list_stays_beside_a_hero() {
        let mut hero = card("session-preview");
        hero.is_hero = true;
        let cards = vec![hero, card("session"), card("recent-games")];
        let t = types(&cards, &arrange(&cards, 0));
        assert_eq!(t[1], "recent-games", "{t:?}");
    }

    #[test]
    fn two_streams_leave_a_recap_the_whole_side_column() {
        let mut recap = card("recap");
        recap.recap_has_content = true;
        assert_eq!(two_stream_start(&[recap, card("session")]), 1);
        assert_eq!(two_stream_start(&[card("recap"), card("session")]), 2);
        assert_eq!(
            two_stream_start(&[card("session"), card("clip"), card("clip")]),
            2
        );
        assert_eq!(two_stream_start(&[card("session")]), 1);
        assert_eq!(two_stream_start(&[]), 0);
    }

    #[test]
    fn after_a_stream_the_grid_is_plain_rows() {
        // Frame D: the recap stands beside the two streams. Then three cells,
        // a cell and the wide grid, then two cells.
        let cards = vec![
            card("recap"),
            card("daily-recap"),
            card("game-session"),
            card("game-session"),
            card("clip"),
            grid(),
            h2h("cell-full"),
            card("invite"),
        ];
        let order = arrange(&cards, 1);
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        let l = layout(&infos, 1);
        assert_eq!(l.lead, Lead::Plain);
        assert_eq!(l.row2_base, 1, "row 2 starts right after the side column");
        assert_eq!(
            wide_slots(&l, infos.len()),
            [5],
            "the grid is the one wide slot"
        );
        assert_eq!(infos[5].card_type, "recent-games");
    }

    fn hero_belt() -> CardInfo {
        h2h("belt")
    }

    #[test]
    fn the_belt_hero_takes_four_cards_and_the_list_the_third_cell_of_row_two() {
        // Frame E: belt hero, head2head cell, daily recap and voice, then two
        // sessions and the list.
        let cards = vec![
            hero_belt(),
            h2h("cell"),
            card("daily-recap"),
            card("voice-idle"),
            card("game-session"),
            card("game-session"),
            card("recent-games"),
        ];
        let order = arrange(&cards, 0);
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        let l = layout(&infos, 0);
        assert_eq!(l.lead, Lead::Belt);
        assert_eq!(l.row2_base, 4);
        assert!(wide_slots(&l, infos.len()).is_empty());
        assert_eq!(infos[6].card_type, "recent-games");
        assert_eq!(arrange(&infos, 0), (0..infos.len()).collect::<Vec<_>>());
    }

    #[test]
    fn a_dormant_feed_stacks_two_cells_beside_two_tall_cards() {
        // Frame F: the server lists the recap first and the cell in the middle.
        let mut recap = card("recap");
        recap.recap_has_content = true;
        let cards = vec![
            recap,
            card("game-session"),
            h2h("cell"),
            card("game-session"),
        ];
        let order = arrange(&cards, 0);
        let t = types(&cards, &order);
        assert_eq!(
            t,
            ["game-session", "game-session", "head2head", "recap"],
            "{t:?}"
        );
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        let l = layout(&infos, 0);
        assert_eq!(l.lead, Lead::TallTrio);
        assert_eq!(l.row2_base, 4);
        assert_eq!(arrange(&infos, 0), (0..infos.len()).collect::<Vec<_>>());
    }

    #[test]
    fn a_hero_keeps_the_feed_when_a_recap_and_a_cell_follow() {
        let mut hero = card("session-preview");
        hero.is_hero = true;
        let mut recap = card("recap");
        recap.recap_has_content = true;
        let cards = vec![hero, h2h("cell"), card("session"), recap];
        let order = arrange(&cards, 0);
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        assert_eq!(layout(&infos, 0).lead, Lead::Hero);
    }

    #[test]
    fn a_live_stream_moves_the_start() {
        assert_eq!(grid_start(1, 5), 2);
        assert_eq!(grid_start(0, 5), 0);
        assert_eq!(grid_start(2, 5), 0);
    }

    fn live() -> CardInfo {
        CardInfo {
            has_poster: true,
            ..card("live-session")
        }
    }

    /// A live session without poster art.
    fn live_cell() -> CardInfo {
        card("live-session")
    }

    #[test]
    fn a_live_session_without_poster_is_a_cell_and_the_wide_head2head_keeps_the_slot() {
        let cards = vec![
            h2h("wide"),
            card("recent-games"),
            card("game-session"),
            live_cell(),
        ];
        let order = arrange(&cards, 0);
        let t = types(&cards, &order);
        assert_eq!(
            &t[..3],
            ["head2head", "live-session", "recent-games"],
            "{t:?}"
        );
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        assert_eq!(wide_slots(&layout(&infos, 0), infos.len())[0], 0);
        assert_eq!(infos[0].variant, "wide");
    }

    #[test]
    fn a_live_session_without_poster_leaves_the_next_card_the_wide_slot() {
        let cards = vec![
            h2h("cell"),
            card("invite"),
            card("game-session"),
            live_cell(),
        ];
        let t = types(&cards, &arrange(&cards, 0));
        assert_eq!(&t[..3], ["head2head", "live-session", "invite"], "{t:?}");
        let mut hero = card("recap");
        hero.is_hero = true;
        let cards = vec![hero, card("recent-games"), live_cell()];
        let t = types(&cards, &arrange(&cards, 0));
        assert_eq!(&t[..3], ["recap", "live-session", "recent-games"], "{t:?}");
    }

    #[test]
    fn a_poster_game_leads_and_a_game_without_one_is_a_cell_behind_the_first_row() {
        let cards = vec![h2h("cell"), card("recent-games"), live_cell(), live()];
        let order = arrange(&cards, 0);
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        assert!(infos[0].has_poster, "{infos:?}");
        assert_eq!(infos[1].card_type, "head2head");
        assert!(infos[2..].iter().any(|c| c.card_type == "live-session"));
        assert_eq!(arrange(&infos, 0), (0..cards.len()).collect::<Vec<_>>());
    }

    #[test]
    fn no_clip_sits_in_a_wide_slot_after_a_live_cell_moves_the_row() {
        let cards = vec![
            h2h("wide"),
            card("recent-games"),
            card("game-session"),
            card("clip"),
            card("clip"),
            card("game-session"),
            card("clip"),
            card("game-session"),
            live_cell(),
        ];
        let order = arrange(&cards, 0);
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        for w in wide_slots(&layout(&infos, 0), infos.len()) {
            assert_ne!(infos[w].card_type, "clip", "{infos:?}");
        }
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
    fn a_live_no_live_a_crew_whose_cell_leads() {
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
    fn a_live_stream_keeps_the_side_column_and_the_live_no_live_the_grid() {
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
