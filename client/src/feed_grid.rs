//! Where the cards of the this_week grid land.
//!
//! `crew_feed.slint` lays cards out by position, not by kind: the slot at a
//! fixed index is wide (two columns), whatever card sits there. Two rules need
//! the position, so they live here and mirror the layout in `crew_feed.slint`
//! (`row1-consumed`, `row2-base` and the row 3 wide slot):
//!
//! - A session of a crew of one stands left of the wide head2head card
//!   (frame B): one cell, then two columns. `Lead::CellWide`.
//! - A joined card (a member who joined in the last 24 hours) is a plain cell.
//!   It stands after the first row of the lead and before the sessions, one for
//!   each joiner. It never takes a wide slot. `Layout::joins_at`.
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
//! - The weekly recap is never a one-cell card. It is the tall card of the
//!   side column beside a hero (two rows high, one column), else the wide card
//!   of the first wide slot after the lead (`place_wide_cards`).
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
    /// The head2head variant: "wide" (a crew of one), "pairs" (two or more),
    /// "versus", "belt", "invite".
    pub variant: String,
    pub is_hero: bool,
    /// A live session with poster art: the only live card for the wide slot.
    pub has_poster: bool,
    /// A recap with content takes the side column alone.
    pub recap_has_content: bool,
    /// A recap with belts: the tall card is three rows high, else two.
    pub recap_belts: bool,
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
            recap_has_content: c.card_type == "recap" && c.recap_rows > 0,
            recap_belts: c.card_type == "recap" && c.recap_rows >= 3,
        }
    }
}

impl CardInfo {
    /// A weekly recap with content: it needs the tall slot or a wide slot,
    /// never one cell.
    fn is_recap(&self) -> bool {
        self.card_type == "recap" && self.recap_has_content
    }

    /// The head2head card, which is always two columns wide: the chips of a
    /// crew of one, or the pair rows of two or more. A versus card and the
    /// belt hero are heroes of their own.
    fn is_h2h_wide(&self) -> bool {
        self.card_type == "head2head" && (self.variant == "wide" || self.variant == "pairs")
    }

    /// A card that takes two columns when the grid has the room: head2head,
    /// the recap and the poster grid.
    fn is_wide_card(&self) -> bool {
        self.is_h2h_wide() || self.is_recap() || self.recent_grid
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
    /// A wide card (the recap or the poster grid) leads row 2: two columns,
    /// then one card.
    r2_wide: bool,
    /// The tall recap is three rows high (it has belts): it stands in the side
    /// column beside a hero, a live stream or head2head and its two cells. The
    /// row after the lead then has two columns, not three.
    tall3: bool,
}

impl Layout {
    /// The cards of row 2: three cells, or the grid and one card. With a tall
    /// recap of three rows it has two columns: two cells, or one wide card.
    fn row2_len(&self) -> usize {
        // Beside a wide lead the third row of the recap is the wide card's own
        // row, so the row after it is full.
        match (self.tall3 && self.lead != Lead::Wide, self.r2_wide) {
            (false, false) => 3,
            (false, true) => 2,
            (true, false) => 2,
            (true, true) => 1,
        }
    }

    /// The first card of row 3.
    fn row3_base(&self) -> usize {
        self.row2_base + self.row2_len()
    }
}

/// Frame F: the head2head card leads, the recap stands beside it, tall in the
/// side column, and two cells sit under the head2head card.
fn tall_trio(cards: &[CardInfo], start: usize) -> bool {
    cards.get(start).is_some_and(CardInfo::is_h2h_wide)
        && cards.get(start + 1).is_some_and(CardInfo::is_recap)
        && cards.len() >= start + 4
}

fn layout(cards: &[CardInfo], start: usize) -> Layout {
    let at = |i: usize| cards.get(i);
    let first = at(start);
    let variant = first
        .filter(|c| c.card_type == "head2head")
        .map(|c| c.variant.as_str())
        .unwrap_or("");
    let first_wide = first.is_some_and(CardInfo::is_h2h_wide);
    let session_then_wide = first.is_some_and(|c| c.card_type == "game-session")
        && at(start + 1).is_some_and(|c| c.card_type == "head2head" && c.variant == "wide");
    let lead = match first {
        None => Lead::Hero,
        Some(_) if session_then_wide => Lead::CellWide,
        Some(_) if variant == "belt" => Lead::Belt,
        Some(c) if !c.is_hero && tall_trio(cards, start) => Lead::TallTrio,
        // Only joined cards: the grid is plain rows of cells.
        Some(c) if c.card_type == "joined" => Lead::Plain,
        Some(c) if c.card_type == "live-session" && c.has_poster => Lead::Wide,
        Some(c) if c.card_type == "clip" && c.is_hero => Lead::FullRow,
        Some(_) if variant == "versus" => Lead::FullRow,
        Some(_) if first_wide => Lead::Wide,
        Some(c) if c.card_type == "invite" => Lead::Plain,
        // After a live stream the grid is plain rows: a card there is a cell.
        Some(c) if start > 0 && !c.is_hero => Lead::Plain,
        Some(_) => Lead::Hero,
    };
    let side_recap = at(start + 1).is_some_and(CardInfo::is_recap);
    // A wide card behind the lead stands in row 2, not in the one-column slot
    // beside the lead: the lead then holds row 1 alone.
    let wide_follows =
        at(start + 1).is_some_and(CardInfo::is_h2h_wide) || (lead == Lead::Wide && side_recap);
    // Beside a hero, the first of the two side slots that holds head2head.
    let h2h_in_side = (1..=2).find(|&k| at(start + k).is_some_and(CardInfo::is_h2h_wide));
    // The recap with belts is the tall card of the side column, three rows
    // high: beside a hero, beside head2head in frame F, or alone beside a live
    // stream (`start` is 1 and the recap leads, see `grid_start`). Beside a
    // stream it needs the cards under the stream to leave the third column
    // free: a wide card (`Lead::Wide`) and a plain row do, a hero and a full
    // row do not, and the recap stays two rows.
    let tall_recap = |i: usize| at(i).is_some_and(|c| c.is_recap() && c.recap_belts);
    let tall3 = matches!(lead, Lead::Hero | Lead::TallTrio) && tall_recap(start + 1)
        || (start == 1 && tall_recap(0) && matches!(lead, Lead::Wide | Lead::Plain));
    let left = cards.len().saturating_sub(start);
    let consumed = if cards.len() <= start {
        0
    } else {
        match lead {
            Lead::Plain => 0,
            Lead::Wide if wide_follows || tall3 => 1,
            Lead::Wide | Lead::CellWide => left.min(2),
            Lead::Belt | Lead::TallTrio => left.min(4),
            Lead::FullRow => 1,
            Lead::Hero if side_recap => left.min(2),
            // Head2head is not a side card: row 2 starts where it stands.
            Lead::Hero if h2h_in_side.is_some() => h2h_in_side.unwrap_or(1),
            Lead::Hero => left.min(3),
        }
    };
    let row2_base = start + consumed;
    Layout {
        lead,
        start,
        row2_base,
        r2_wide: at(row2_base).is_some_and(CardInfo::is_wide_card),
        tall3,
    }
}

/// Where the joined cards go among `len` other cards: the first card after the
/// first row of the lead (row 2), else after the first three cells of a plain
/// grid, which has no hero.
fn joins_at(l: &Layout, len: usize) -> usize {
    let at = if l.lead == Lead::Plain {
        // A wide card leads the first row: the join is the cell beside it.
        if l.r2_wide {
            l.start + 1
        } else {
            l.start + 3
        }
    } else {
        // The cell beside the wide card that leads row 2, never its slot.
        l.row2_base + usize::from(l.r2_wide)
    };
    at.min(len)
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
    if l.lead == Lead::TallTrio && l.start < len {
        w.push(l.start); // the head2head card, the recap tall beside it
    }
    if l.lead == Lead::Belt && l.start + 1 < len {
        w.push(l.start); // the belt hero
        w.push(l.start + 1); // the head2head card in row 2
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
/// Columns of the Crew history grid.
const MEMORY_COLS: usize = 3;

/// Put the cards of Crew history in a three-column grid and return the rows it
/// needs. A weekly recap is the tall card here too: one column, `recap_rows`
/// rows (two, three with belts). Every other card is one cell. Each card takes
/// the first free cell in reading order that fits it, so the cells beside a
/// tall recap fill with the cards that follow. Sets `mem_col`, `mem_row` and
/// `mem_span`.
pub(crate) fn place_memory(mut cards: Vec<FeedCardData>) -> (Vec<FeedCardData>, i32) {
    let mut taken: Vec<[bool; MEMORY_COLS]> = Vec::new();
    for card in &mut cards {
        let span = if card.card_type == "recap" {
            card.recap_rows.clamp(2, 3) as usize
        } else {
            1
        };
        let mut row = 0;
        let col = loop {
            taken.resize(taken.len().max(row + span), [false; MEMORY_COLS]);
            if let Some(c) = (0..MEMORY_COLS).find(|&c| (row..row + span).all(|r| !taken[r][c])) {
                break c;
            }
            row += 1;
        };
        for r in row..row + span {
            taken[r][col] = true;
        }
        card.mem_col = col as i32;
        card.mem_row = row as i32;
        card.mem_span = span as i32;
    }
    (cards, taken.len() as i32)
}

#[cfg(test)]
pub fn wide_slot_indices(cards: &[CardInfo], start: usize) -> Vec<usize> {
    wide_slots(&layout(cards, start), cards.len())
}

/// Cards that keep their place: they decide the layout or are a fixed item of
/// it. Everything else may swap with a clip.
fn is_fixed(t: &str) -> bool {
    matches!(
        t,
        "clip"
            | "head2head"
            | "invite"
            | "recap"
            | "recent-games"
            | "locked"
            | "live-session"
            | "joined"
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
        // The games moved the cards behind them: the wide cards find their
        // slots again.
        place_wide_cards(cards, &mut out, start);
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
        // A recap holds the side column alone and the wide cards stand in
        // their slots: `place_more` puts them right once the session is in.
        out.insert((start + 1).min(out.len()), live);
        return place_more(out);
    }
    // The live session leads. The head2head card is always two columns wide, so
    // it is not the cell beside it: it takes the first wide slot of row 2.
    let mut seq = rest;
    seq.insert(start.min(seq.len()), live);
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
    let side_recap = out.get(start + 1).is_some_and(|&i| cards[i].is_recap());
    let base = start + 1 + usize::from(side_recap);
    for (n, &i) in lives.iter().enumerate() {
        out.insert((base + n).min(out.len()), i);
    }
    place_wide_cards(cards, &mut out, start);
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

    // The joined cards wait outside the order: the rules below place the other
    // cards as if there were none, and the joins go in after the first row.
    let joins: Vec<usize> = order
        .iter()
        .copied()
        .filter(|&i| cards[i].card_type == "joined")
        .collect();
    order.retain(|i| !joins.contains(i));

    // 0. A session of a crew of one leads the wide head2head card (frame B).
    if no_live
        && cards
            .get(start)
            .is_some_and(|c| c.card_type == "head2head" && c.variant == "wide")
    {
        // The side column of a live stream (before `start`) keeps its cards.
        if let Some(p) = order
            .iter()
            .skip(start)
            .position(|&i| cards[i].card_type == "game-session")
            .map(|p| p + start)
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

    // 1. The wide cards go to their slots: head2head, the recap, recent games.
    place_wide_cards(cards, &mut order, start);

    // 2. The joined cards: after the first row, before the sessions. They take
    // a cell, never a wide slot, so the wide cards find their slots again.
    if !joins.is_empty() {
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        let at = joins_at(&layout(&infos, start), order.len());
        for (n, &i) in joins.iter().enumerate() {
            order.insert((at + n).min(order.len()), i);
        }
        place_wide_cards(cards, &mut order, start);
    }

    clips_leave_wide(cards, &mut order, start);
    order
}

fn infos_of(cards: &[CardInfo], order: &[usize]) -> Vec<CardInfo> {
    order.iter().map(|&i| cards[i].clone()).collect()
}

/// The wide cards to their slots: head2head, the recap and recent games.
///
/// - Head2head is always two columns wide. It leads a feed with no hero. Else it
///   takes the first wide slot after the lead block.
/// - The recap is the tall card of the side column beside a hero, else the wide
///   card of the next wide slot. Never one cell.
/// - Recent games is on the first screen for every crew size. Under a hero or
///   in a plain grid it takes the wide slot of row 3 (frames C and D). A wide
///   card in row 2 pushes it one card up the order.
///
/// A wide card that is not at home leaves the order, the lead is read from the
/// other cards, and each card goes back in at its slot: head2head first, then
/// the recap, then recent games.
fn place_wide_cards(cards: &[CardInfo], order: &mut Vec<usize>, start: usize) {
    if order.len() <= start {
        return;
    }
    let l0 = layout(&infos_of(cards, order), start);
    // Cards that already hold their slot as part of the lead block.
    let h2h_home = |o: &[usize], l: &Layout| match l.lead {
        Lead::Wide | Lead::TallTrio => o.get(start).is_some_and(|&i| cards[i].is_h2h_wide()),
        Lead::CellWide => o.get(start + 1).is_some_and(|&i| cards[i].is_h2h_wide()),
        Lead::Belt => o.get(start + 1).is_some_and(|&i| cards[i].is_h2h_wide()),
        _ => false,
    };
    let h2h = order
        .iter()
        .copied()
        .find(|&i| cards[i].is_h2h_wide())
        .filter(|_| !h2h_home(order, &l0));
    // The tall trio already holds its recap, tall. So does the side column of
    // one live stream, when the recap leads the feed (`grid_start`).
    let side_tall = start == 1 && order.first().is_some_and(|&i| cards[i].is_recap());
    let recap = if l0.lead == Lead::TallTrio || side_tall {
        None
    } else {
        order.iter().copied().find(|&i| cards[i].is_recap())
    };
    let games = order
        .iter()
        .copied()
        .find(|&i| cards[i].card_type == "recent-games");
    order.retain(|&i| Some(i) != h2h && Some(i) != recap && Some(i) != games);

    // The wide slot of row 3 is the second card of the row: row 3 holds one
    // card and a wide card.
    let r3_wide = |l: &Layout| l.row3_base() + 1;
    // The first free wide slot: row 2, else row 3.
    let next_slot = |_: &[usize], l: &Layout| {
        if l.r2_wide {
            r3_wide(l)
        } else {
            l.row2_base
        }
    };

    // Beside a hero the recap is the tall card of the side column. It goes in
    // first: it moves the wide slot of row 2 one card up.
    let mut recap = recap;
    if let Some(id) = recap {
        if layout(&infos_of(cards, order), start).lead == Lead::Hero {
            order.insert((start + 1).min(order.len()), id);
            recap = None;
        }
    }

    if let Some(id) = h2h {
        let l = layout(&infos_of(cards, order), start);
        let at = next_slot(order, &l);
        order.insert(at.min(order.len()), id);
    }

    if let Some(id) = recap {
        let l = layout(&infos_of(cards, order), start);
        let at = next_slot(order, &l);
        order.insert(at.min(order.len()), id);
    }

    if let Some(id) = games {
        let infos = infos_of(cards, order);
        let l = layout(&infos, start);
        let grid = cards[id].recent_grid;
        let r3 = r3_wide(&l);
        let r3_taken = order.get(r3).is_some_and(|&i| cards[i].is_wide_card());
        let side_recap =
            l.lead == Lead::Hero && infos.get(start + 1).is_some_and(CardInfo::is_recap);
        let target = if l.r2_wide && !matches!(l.lead, Lead::TallTrio | Lead::Belt) {
            // A wide card leads row 2: the grid takes the wide slot of row 3,
            // the list is the cell beside the wide card.
            if grid && !r3_taken {
                r3
            } else {
                // The cell beside the wide card, behind a live session that is
                // already there.
                let mut at = l.row2_base + 1;
                while infos.get(at).is_some_and(|c| c.card_type == "live-session") {
                    at += 1;
                }
                at
            }
        } else {
            match l.lead {
                // The wide slot of the second row, with the invite card and the
                // cells in the first.
                Lead::Plain if r3_taken => r3 + 1,
                Lead::Plain => r3,
                // The session cell and the wide card fill row 1. The list is the
                // first cell of row 2, with the skeletons beside it.
                Lead::CellWide => start + 2,
                // The third cell of row 2, beside the two sessions (frame E).
                Lead::Belt | Lead::TallTrio => l.row2_base + 2,
                // The poster grid needs two columns. Under a hero it takes the
                // wide slot of row 3 (frames C and D). The list stays in the side
                // column, behind a tall recap.
                Lead::Hero if grid => r3,
                Lead::Hero if side_recap => l.row2_base,
                // The grid leads row 2 under a wide lead. The list is the cell
                // beside it, behind the invite card of a big crew.
                Lead::Wide if grid => l.row2_base,
                Lead::Wide
                    if infos
                        .get(start + 1)
                        .is_some_and(|c| c.card_type == "invite") =>
                {
                    start + 2
                }
                // The list is a cell beside the lead, behind a live session that
                // is already the first one.
                Lead::Wide | Lead::Hero | Lead::FullRow => {
                    start
                        + 1
                        + usize::from(
                            infos
                                .get(start + 1)
                                .is_some_and(|c| c.card_type == "live-session"),
                        )
                }
            }
        };
        order.insert(target.min(order.len()), id);
    }
}

/// A feed with no hero: the head2head card leads, a recap with content stands
/// second so it is the tall card beside it, and two plain cells sit under the
/// head2head card (frame F, a crew that was away). A feed that already has a
/// lead of its own keeps it.
fn tall_trio_order(cards: &[CardInfo], order: &mut Vec<usize>, start: usize) {
    let Some(&h) = order.get(start) else {
        return;
    };
    if !cards[h].is_h2h_wide() {
        return;
    }
    // A recap before `start` is the side card of a live stream: it stays.
    let Some(&r) = order.iter().skip(start).find(|&&i| cards[i].is_recap()) else {
        return;
    };
    // Two plain cells to sit under the head2head card: anything that is not a
    // fixed card.
    // The side column of a live stream (before `start`) keeps its cards.
    let plain: Vec<usize> = order
        .iter()
        .skip(start)
        .copied()
        .filter(|&i| !is_fixed(&cards[i].card_type))
        .take(2)
        .collect();
    if plain.len() < 2 {
        return;
    }
    order.retain(|i| !plain.contains(i) && *i != h && *i != r);
    let mut head = vec![h, r];
    head.extend(plain);
    for (n, i) in head.into_iter().enumerate() {
        order.insert((start + n).min(order.len()), i);
    }
}

/// A clip and a joined card leave every wide slot. Swap it with the nearest card that is not
/// fixed, later cards first. With none, the clip stays.
fn clips_leave_wide(cards: &[CardInfo], order: &mut [usize], start: usize) {
    if order.len() <= start {
        return;
    }
    let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
    let l = layout(&infos, start);
    for w in wide_slots(&l, order.len()) {
        // A clip is a card that needs a cell of its own and the joined card is
        // the smallest card: neither fills two columns.
        if !matches!(infos[w].card_type.as_str(), "clip" | "joined") {
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

/// Arrange the cards for `live_streams` streams beside the feed. One live
/// stream takes the first cards of the feed into its side column, one column
/// wide. A recap with content then leads and holds the column alone, tall; else
/// two cards stand in it, and head2head is never one of them: it is always two
/// columns wide. Mirrors `stream-side-consumed` in `crew_feed.slint`.
pub fn arrange_for_streams(mut cards: Vec<FeedCardData>, live_streams: usize) -> Vec<FeedCardData> {
    if live_streams == 1 {
        let infos: Vec<CardInfo> = cards.iter().map(CardInfo::of).collect();
        if let Some(p) = infos.iter().position(CardInfo::is_recap).filter(|&p| p > 0) {
            let recap = cards.remove(p);
            cards.insert(0, recap);
        } else if !infos.first().is_some_and(CardInfo::is_recap) {
            // The first two cards that are not head2head stand in the column.
            let side: Vec<usize> = (0..infos.len())
                .filter(|&i| !infos[i].is_h2h_wide())
                .take(2)
                .collect();
            let mut slots: Vec<Option<FeedCardData>> = cards.into_iter().map(Some).collect();
            let mut out: Vec<FeedCardData> = side.iter().filter_map(|&i| slots[i].take()).collect();
            out.extend(slots.into_iter().flatten());
            cards = out;
        }
    }
    let infos: Vec<CardInfo> = cards.iter().map(CardInfo::of).collect();
    let start = grid_start(live_streams, &infos);
    arrange_cards(cards, start)
}

/// The first grid card: one live stream takes the first cards of the feed into
/// its side column (`stream-side-consumed` in `crew_feed.slint`): the recap
/// alone when it leads, else up to two cards that are not head2head.
pub fn grid_start(live_streams: usize, cards: &[CardInfo]) -> usize {
    if live_streams == 1 {
        match cards.first() {
            Some(c) if c.is_recap() => 1,
            _ => cards
                .iter()
                .take(2)
                .take_while(|c| !c.is_h2h_wide())
                .count(),
        }
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
    Rc::new(VecModel::from(arrange_for_streams(cards, live_streams))).into()
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
            h2h("pairs"),
            card("invite"),
            card("clip"),
            card("game-session"),
            card("clip"),
            card("clip"),
            card("clip"),
            grid(),
            card("clip"),
        ]
    }

    #[test]
    fn head2head_leads_and_recent_games_takes_the_wide_slot_of_row_two() {
        // The wide head2head card and the invite card are row 1. The poster
        // grid leads row 2, and clips fill the cells.
        let cards = fifty();
        let order = arrange(&cards, 0);
        let t = types(&cards, &order);
        assert_eq!(&t[..3], ["head2head", "invite", "recent-games"], "{t:?}");
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        let l = layout(&infos, 0);
        assert_eq!(l.lead, Lead::Wide);
        assert_eq!(wide_slots(&l, infos.len())[..2], [0, 2]);
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
            h2h("pairs"),
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
        // Row 1: head2head and the invite card. Rows 2 and 3 hold cells, and
        // the second slot of row 3 is wide: a clip there swaps with a session.
        let cards = vec![
            h2h("pairs"),
            card("invite"),
            card("clip"),
            card("clip"),
            card("clip"),
            card("clip"),
            card("clip"),
            card("clip"),
            card("game-session"),
        ];
        let order = arrange(&cards, 0);
        assert_eq!(types(&cards, &order)[6], "game-session");
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
        // Frame C: the stream snapshot hero and two side cards, then head2head
        // in the wide slot of row 2 with a cell, and the grid in row 3.
        let mut hero = card("session-preview");
        hero.is_hero = true;
        let cards = vec![
            hero,
            h2h("pairs"),
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
        // Head2head leads row 2 (two columns, always), the grid row 3.
        assert_eq!(wide, [0, 3, 6], "the hero, head2head and the grid");
        assert_eq!(infos[3].card_type, "head2head");
        assert_eq!(infos[6].card_type, "recent-games");
        assert_eq!(infos[7].card_type, "clip", "a clip is a cell, never wide");
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
        // Frame D: the recap stands beside the two streams. Head2head leads the
        // first row of the grid with the invite card beside it, then the grid.
        let cards = vec![
            card("recap"),
            card("daily-recap"),
            card("game-session"),
            card("game-session"),
            card("clip"),
            grid(),
            h2h("pairs"),
            card("invite"),
        ];
        let order = arrange(&cards, 1);
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        let l = layout(&infos, 1);
        assert_eq!(l.lead, Lead::Wide);
        assert_eq!(infos[1].card_type, "head2head", "head2head leads the grid");
        assert_eq!(l.row2_base, 3, "row 2 follows the lead row");
        let wide = wide_slots(&l, infos.len());
        assert_eq!(wide[..2], [1, 3], "head2head, then the grid leads row 2");
        assert_eq!(infos[3].card_type, "recent-games");
    }

    fn hero_belt() -> CardInfo {
        h2h("belt")
    }

    #[test]
    fn the_belt_hero_takes_four_cards_and_the_list_the_third_cell_of_row_two() {
        // Frame E: the belt hero and head2head are the two wide cards, the daily
        // recap and voice sit beside them, then two sessions and the list.
        let cards = vec![
            hero_belt(),
            h2h("pairs"),
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
        assert_eq!(wide_slots(&l, infos.len()), [0, 1], "belt hero, head2head");
        assert_eq!(infos[6].card_type, "recent-games");
        assert_eq!(arrange(&infos, 0), (0..infos.len()).collect::<Vec<_>>());
    }

    #[test]
    fn a_dormant_feed_stacks_two_cells_beside_two_tall_cards() {
        // Frame F: head2head leads a feed with no hero. The recap is the tall
        // card beside it, and two cells sit under head2head.
        let mut recap = card("recap");
        recap.recap_has_content = true;
        let cards = vec![
            h2h("pairs"),
            card("game-session"),
            recap,
            card("game-session"),
        ];
        let order = arrange(&cards, 0);
        let t = types(&cards, &order);
        assert_eq!(
            t,
            ["head2head", "recap", "game-session", "game-session"],
            "{t:?}"
        );
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        let l = layout(&infos, 0);
        assert_eq!(l.lead, Lead::TallTrio);
        assert_eq!(l.row2_base, 4);
        assert_eq!(wide_slots(&l, infos.len()), [0], "head2head is two columns");
        assert_eq!(arrange(&infos, 0), (0..infos.len()).collect::<Vec<_>>());
    }

    fn belts_recap() -> CardInfo {
        CardInfo {
            recap_belts: true,
            ..recap()
        }
    }

    #[test]
    fn a_recap_with_belts_is_three_rows_beside_a_hero_and_the_row_under_it_has_two_columns() {
        let mut hero = card("session-preview");
        hero.is_hero = true;
        let cards = vec![
            hero,
            belts_recap(),
            card("session"),
            card("session"),
            card("session"),
            card("session"),
        ];
        let order = arrange(&cards, 0);
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        let l = layout(&infos, 0);
        assert_eq!(l.lead, Lead::Hero);
        assert!(l.tall3, "belts make the recap three rows");
        assert_eq!(l.row2_base, 2);
        assert_eq!(
            l.row2_len(),
            2,
            "two cells under the hero, the recap holds the third column"
        );
        assert_eq!(l.row3_base(), 4);
        // Without belts the recap is two rows and the row under the hero has three.
        let mut plain = cards.clone();
        plain[1] = recap();
        let order = arrange(&plain, 0);
        let infos: Vec<CardInfo> = order.iter().map(|&i| plain[i].clone()).collect();
        let l = layout(&infos, 0);
        assert!(!l.tall3);
        assert_eq!(l.row2_len(), 3);
    }

    #[test]
    fn a_wide_card_under_a_three_row_recap_is_the_only_card_of_its_row() {
        let mut hero = card("session-preview");
        hero.is_hero = true;
        let cards = vec![
            hero,
            belts_recap(),
            h2h("pairs"),
            card("session"),
            card("session"),
            card("session"),
        ];
        let order = arrange(&cards, 0);
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        let l = layout(&infos, 0);
        assert!(l.tall3 && l.r2_wide);
        assert_eq!(infos[2].card_type, "head2head");
        assert_eq!(
            l.row2_len(),
            1,
            "head2head fills the two columns beside the recap"
        );
        assert_eq!(l.row3_base(), 3);
        assert_eq!(wide_slots(&l, infos.len())[..2], [0, 2]);
    }

    #[test]
    fn the_tall_trio_recap_with_belts_is_three_rows() {
        let cards = vec![
            h2h("pairs"),
            card("game-session"),
            belts_recap(),
            card("game-session"),
            card("session"),
            card("session"),
        ];
        let order = arrange(&cards, 0);
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        let l = layout(&infos, 0);
        assert_eq!(l.lead, Lead::TallTrio);
        assert!(l.tall3);
        assert_eq!(l.row2_base, 4);
        assert_eq!(l.row2_len(), 2);
    }

    #[test]
    fn beside_a_stream_the_recap_is_three_rows_only_under_a_wide_or_plain_lead() {
        // The stream takes the recap alone (start 1). Head2head leads the grid:
        // its own row is the recap's third row, so the row after is full.
        let cards = vec![
            belts_recap(),
            h2h("pairs"),
            card("session"),
            card("session"),
        ];
        let order = arrange(&cards, 1);
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        let l = layout(&infos, 1);
        assert_eq!(l.lead, Lead::Wide);
        assert!(l.tall3);
        assert_eq!(
            l.row2_base, 2,
            "head2head alone in its row, no cell beside it"
        );
        assert_eq!(l.row2_len(), 3);
        // Plain rows under the stream: the first row has two columns.
        let cards = vec![
            belts_recap(),
            card("session"),
            card("session"),
            card("session"),
        ];
        let order = arrange(&cards, 1);
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        let l = layout(&infos, 1);
        assert_eq!(l.lead, Lead::Plain);
        assert!(l.tall3);
        assert_eq!(l.row2_len(), 2);
        // A hero under the stream cannot leave the third column: two rows.
        let mut hero = card("clip");
        hero.is_hero = true;
        let cards = vec![belts_recap(), hero, card("session"), card("session")];
        let order = arrange(&cards, 1);
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        let l = layout(&infos, 1);
        assert!(!l.tall3);
    }

    #[test]
    fn a_hero_keeps_the_feed_when_a_recap_and_a_cell_follow() {
        let mut hero = card("session-preview");
        hero.is_hero = true;
        let mut recap = card("recap");
        recap.recap_has_content = true;
        let cards = vec![hero, h2h("pairs"), card("session"), recap];
        let order = arrange(&cards, 0);
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        assert_eq!(layout(&infos, 0).lead, Lead::Hero);
    }

    #[test]
    fn a_live_stream_moves_the_start() {
        let five: Vec<CardInfo> = (0..5).map(|_| card("clip")).collect();
        assert_eq!(grid_start(1, &five), 2);
        assert_eq!(grid_start(0, &five), 0);
        assert_eq!(grid_start(2, &five), 0);
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
            h2h("pairs"),
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
        let cards = vec![h2h("pairs"), card("recent-games"), live_cell(), live()];
        let order = arrange(&cards, 0);
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        assert!(infos[0].has_poster, "{infos:?}");
        assert_eq!(
            infos[1].card_type, "live-session",
            "the cell beside the lead"
        );
        assert_eq!(infos[2].card_type, "head2head", "two columns of row 2");
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
    fn a_live_session_takes_the_wide_slot_and_head2head_the_wide_slot_of_row_two() {
        let cards = vec![
            h2h("wide"),
            card("recent-games"),
            card("game-session"),
            live(),
        ];
        let order = arrange(&cards, 0);
        let t = types(&cards, &order);
        assert_eq!(t[0], "live-session", "{t:?}");
        assert_eq!(t[1], "game-session", "the cell beside the live session");
        assert_eq!(t[2], "head2head", "{t:?}");
        assert_eq!(t[3], "recent-games", "{t:?}");
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        assert_eq!(wide_slots(&layout(&infos, 0), infos.len())[..2], [0, 2]);
    }

    #[test]
    fn a_live_session_leads_a_big_crew_and_head2head_is_wide_in_row_two() {
        let cards = vec![h2h("pairs"), card("invite"), card("game-session"), live()];
        let t = types(&cards, &arrange(&cards, 0));
        assert_eq!(&t[..3], ["live-session", "invite", "head2head"], "{t:?}");
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
            h2h("pairs"),
            card("recent-games"),
            card("game-session"),
            live(),
            live(),
        ];
        let order = arrange(&cards, 0);
        let t = types(&cards, &order);
        assert_eq!(
            &t[..4],
            ["live-session", "game-session", "head2head", "live-session"],
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

    fn joined() -> CardInfo {
        card("joined")
    }

    #[test]
    fn a_join_stands_beside_the_poster_grid_under_the_versus_hero_before_the_session() {
        // Frame B2: the versus hero, then the grid and one card, then the rest.
        let cards = vec![h2h("versus"), grid(), card("game-session"), joined()];
        let order = arrange(&cards, 0);
        assert_eq!(
            types(&cards, &order),
            ["head2head", "recent-games", "joined", "game-session"]
        );
    }

    #[test]
    fn a_join_follows_the_first_row_of_every_lead() {
        // A wide card of a crew of one: the row holds the wide card and a cell.
        let cards = vec![h2h("wide"), card("recent-games"), joined()];
        let t = types(&cards, &arrange(&cards, 0));
        assert_eq!(t[2], "joined", "{t:?}");
        // A hero: the hero and the two side cells are the first row. Head2head
        // leads row 2 and the join is the cell beside it.
        let mut hero = card("session-preview");
        hero.is_hero = true;
        let cards = vec![
            hero,
            h2h("pairs"),
            card("game-session"),
            card("game-session"),
            joined(),
        ];
        let t = types(&cards, &arrange(&cards, 0));
        assert_eq!(&t[3..], ["head2head", "joined"], "{t:?}");
        // Head2head and the invite card are row 1: the join leads row 2.
        let cards = vec![
            h2h("pairs"),
            card("invite"),
            card("game-session"),
            card("game-session"),
            joined(),
        ];
        let t = types(&cards, &arrange(&cards, 0));
        assert_eq!(t[2], "joined", "{t:?}");
    }

    #[test]
    fn no_join_sits_in_a_wide_slot_and_three_joins_all_come_before_the_sessions() {
        let cards = vec![
            h2h("versus"),
            grid(),
            card("game-session"),
            card("game-session"),
            card("game-session"),
            joined(),
            joined(),
            joined(),
        ];
        let order = arrange(&cards, 0);
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        let wide = wide_slots(&layout(&infos, 0), infos.len());
        for w in wide {
            assert_ne!(infos[w].card_type, "joined", "{:?}", types(&cards, &order));
        }
        assert_eq!(
            infos.iter().filter(|c| c.card_type == "joined").count(),
            3,
            "none is dropped"
        );
    }

    #[test]
    fn a_feed_of_joins_alone_is_plain_cells() {
        let cards = vec![joined(), joined()];
        let order = arrange(&cards, 0);
        assert_eq!(order, [0, 1]);
        assert_eq!(layout(&cards, 0).lead, Lead::Plain);
    }

    #[test]
    fn a_join_does_not_move_the_cards_the_other_rules_place() {
        // The poster grid keeps the wide slot of row 3 under a hero (frame C).
        let mut hero = card("session-preview");
        hero.is_hero = true;
        let cards = vec![
            hero,
            h2h("pairs"),
            card("session"),
            card("game-session"),
            card("game-session"),
            card("daily-recap"),
            card("clip"),
            grid(),
            joined(),
        ];
        let order = arrange(&cards, 0);
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        let wide = wide_slots(&layout(&infos, 0), infos.len());
        assert!(
            wide.iter().any(|&w| infos[w].card_type == "recent-games"),
            "{:?}",
            types(&cards, &order)
        );
    }

    fn recap() -> CardInfo {
        CardInfo {
            recap_has_content: true,
            ..card("recap")
        }
    }

    /// Where the first recap with content stands: the tall slot of the side
    /// column (hero lead, tall trio) or a wide slot. `None` is one cell.
    fn recap_shape(cards: &[CardInfo], order: &[usize], start: usize) -> Option<&'static str> {
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        let at = infos.iter().position(CardInfo::is_recap)?;
        let l = layout(&infos, start);
        if l.lead == Lead::Hero && at == start + 1 {
            return Some("tall");
        }
        if l.lead == Lead::TallTrio && at == start + 1 {
            return Some("tall");
        }
        if start == 1 && at == 0 {
            return Some("tall");
        }
        wide_slots(&l, infos.len()).contains(&at).then_some("wide")
    }

    #[test]
    fn a_recap_is_never_one_cell() {
        let mut hero = card("session-preview");
        hero.is_hero = true;
        let leads: Vec<Vec<CardInfo>> = vec![
            vec![h2h("versus")],
            vec![h2h("wide")],
            vec![h2h("pairs"), card("invite")],
            vec![hero.clone()],
            vec![live()],
            vec![card("game-session"), h2h("wide")],
        ];
        for lead in leads {
            for pos in 0..4 {
                let mut cards = lead.clone();
                for _ in 0..4 {
                    cards.push(card("game-session"));
                }
                cards.push(card("clip"));
                cards.push(grid());
                cards.insert((lead.len() + pos).min(cards.len()), recap());
                let order = arrange(&cards, 0);
                assert_eq!(order.len(), cards.len());
                let shape = recap_shape(&cards, &order, 0);
                assert!(
                    shape.is_some(),
                    "recap at {pos} after {:?} is one cell: {:?}",
                    types(&cards, &(0..lead.len()).collect::<Vec<_>>()),
                    types(&cards, &order)
                );
            }
        }
    }

    #[test]
    fn a_recap_goes_to_the_side_column_beside_a_hero() {
        let mut hero = card("session-preview");
        hero.is_hero = true;
        let cards = vec![
            hero,
            card("game-session"),
            card("game-session"),
            card("game-session"),
            recap(),
        ];
        let order = arrange(&cards, 0);
        let t = types(&cards, &order);
        assert_eq!(t[1], "recap", "{t:?}");
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        assert_eq!(layout(&infos, 0).row2_base, 2, "the recap holds the column");
        assert_eq!(arrange(&infos, 0), (0..infos.len()).collect::<Vec<_>>());
    }

    #[test]
    fn a_recap_takes_the_first_wide_slot_of_the_next_row_when_the_column_is_taken() {
        // A versus hero is one full row: row 2 leads with the recap, two
        // columns, and the poster grid takes the wide slot of row 3.
        let cards = vec![
            h2h("versus"),
            card("game-session"),
            card("game-session"),
            grid(),
            recap(),
        ];
        let order = arrange(&cards, 0);
        let t = types(&cards, &order);
        assert_eq!(t[1], "recap", "{t:?}");
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        let l = layout(&infos, 0);
        assert!(l.r2_wide);
        let wide = wide_slots(&l, infos.len());
        assert_eq!(wide[0], 1, "row 2 leads with the recap");
        assert_eq!(arrange(&infos, 0), (0..infos.len()).collect::<Vec<_>>());
    }

    #[test]
    fn one_live_stream_gives_the_recap_the_whole_side_column() {
        let cards = vec![card("game-session"), card("clip"), recap(), card("session")];
        let infos = cards.clone();
        assert_eq!(grid_start(1, &infos), 2, "no recap first: two cards");
        let recap_first = vec![recap(), card("game-session")];
        assert_eq!(grid_start(1, &recap_first), 1, "the recap alone");
        assert_eq!(grid_start(2, &recap_first), 0);
    }

    /// Head2head is always two columns wide: its slot is a wide slot, whatever
    /// leads the feed, wherever the server lists it and however few cards the
    /// feed has.
    #[test]
    fn head2head_is_always_in_a_wide_slot() {
        let mut hero = card("session-preview");
        hero.is_hero = true;
        let leads: Vec<(Vec<CardInfo>, usize)> = vec![
            (vec![], 0),
            (vec![hero.clone()], 0),
            (vec![live()], 0),
            (vec![live_cell()], 0),
            (vec![card("clip")], 0),
            (vec![joined()], 0),
            (vec![card("invite")], 0),
            (vec![card("game-session"), card("clip")], 2),
            (vec![recap(), card("clip")], 1),
        ];
        for variant in ["pairs", "wide"] {
            for (lead, start) in &leads {
                for extra in [0usize, 1, 2, 5] {
                    for pos in 0..3 {
                        let mut cards = lead.clone();
                        for _ in 0..extra {
                            cards.push(card("game-session"));
                        }
                        cards.insert((lead.len() + pos).min(cards.len()), h2h(variant));
                        cards.push(recap());
                        cards.push(grid());
                        let order = arrange(&cards, *start);
                        let infos: Vec<CardInfo> =
                            order.iter().map(|&i| cards[i].clone()).collect();
                        let at = infos
                            .iter()
                            .position(|c| c.card_type == "head2head")
                            .expect("head2head");
                        let l = layout(&infos, *start);
                        assert!(
                            wide_slots(&l, infos.len()).contains(&at),
                            "{variant}: head2head at {at} is one column: {:?} lead {:?} start {start} extra {extra} pos {pos} from {:?}",
                            types(&cards, &order),
                            l.lead,
                            types(&cards, &(0..cards.len()).collect::<Vec<_>>())
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn head2head_takes_the_first_wide_slot_after_the_hero() {
        let mut hero = card("session-preview");
        hero.is_hero = true;
        let cards = vec![hero, card("session"), card("session"), h2h("pairs"), grid()];
        let order = arrange(&cards, 0);
        let t = types(&cards, &order);
        // Row 2 leads with head2head, the grid has the wide slot of row 3.
        assert_eq!(t[3], "head2head", "{t:?}");
        let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
        assert_eq!(wide_slots(&layout(&infos, 0), infos.len())[..2], [0, 3]);
    }

    #[test]
    fn head2head_is_not_a_side_card_of_a_small_feed_behind_a_hero() {
        // Hero, head2head and the invite card of a crew of five: nothing else.
        let mut hero = card("session-preview");
        hero.is_hero = true;
        for cards in [
            vec![hero.clone(), h2h("pairs"), card("invite")],
            vec![hero.clone(), card("invite"), h2h("pairs")],
            vec![hero.clone(), h2h("pairs")],
        ] {
            let order = arrange(&cards, 0);
            let infos: Vec<CardInfo> = order.iter().map(|&i| cards[i].clone()).collect();
            let l = layout(&infos, 0);
            let at = infos
                .iter()
                .position(|c| c.card_type == "head2head")
                .expect("head2head");
            assert!(
                wide_slots(&l, infos.len()).contains(&at),
                "{:?}",
                types(&cards, &order)
            );
        }
    }

    fn history_card(t: &str, rows: i32) -> FeedCardData {
        FeedCardData {
            card_type: t.into(),
            recap_rows: rows,
            ..Default::default()
        }
    }

    fn cells(cards: Vec<FeedCardData>) -> (Vec<(i32, i32, i32)>, i32) {
        let (cards, rows) = place_memory(cards);
        (
            cards
                .iter()
                .map(|c| (c.mem_col, c.mem_row, c.mem_span))
                .collect(),
            rows,
        )
    }

    #[test]
    fn a_recap_in_crew_history_is_one_column_and_two_or_three_rows() {
        let (at, rows) = cells(vec![
            history_card("recap", 3),
            history_card("clip", 0),
            history_card("clip", 0),
            history_card("recap", 2),
            history_card("clip", 0),
        ]);
        // The clips fill the cells beside the tall recap, then the next recap.
        assert_eq!(
            at,
            vec![(0, 0, 3), (1, 0, 1), (2, 0, 1), (1, 1, 2), (2, 1, 1)]
        );
        assert_eq!(rows, 3);
    }

    #[test]
    fn crew_history_without_a_recap_is_rows_of_three() {
        let (at, rows) = cells((0..4).map(|_| history_card("clip", 0)).collect());
        assert_eq!(at, vec![(0, 0, 1), (1, 0, 1), (2, 0, 1), (0, 1, 1)]);
        assert_eq!(rows, 2);
        assert_eq!(cells(vec![]).1, 0);
    }
}
