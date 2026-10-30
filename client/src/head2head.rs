//! Head2head on the crew feed: map the `head2head` entry of `this_week` to
//! what the UI draws. Plan `next-gen-feed.md` sections 3, 4 and 9, spec
//! `features/HEAD2HEAD.md`.
//!
//! The payload names members by user id only. Names and hues come from the
//! roster the caller passes in, so the code here has no window and tests
//! need none.

use serde::Deserialize;
use serde_json::Value;

use crate::converters::{avatar_color_index, make_initials};
use crate::sealed::Seal;
use crate::{H2hPairRow, H2hPerson, Head2headData, TitleHeld};

/// Seats head2head shows, as in the backend. A crew of five or more has none.
pub const SEATS: i32 = 5;
/// The invite card shows for 5 to 9 members. From 10 it is not emitted: the
/// crew menu keeps the share link.
pub const INVITE_CARD_MAX_MEMBERS: i32 = 9;
/// Most pair rows on the card: two columns of two. The card is always two grid
/// columns wide. A "+N more" line counts the pairs that do not fit.
pub const PAIR_ROWS: usize = 6;
/// From five members the pairs card needs this many contested pairs. Fewer
/// make an empty two-column card; the crew's belts then show in the recaps.
pub const MIN_PAIR_ROWS: usize = 2;
/// Most octagons on the invite card. More members show as "+N".
pub const INVITE_CARD_MEMBERS: usize = 8;
/// Chips on the wide card and the versus card.
const CHIPS: usize = 4;
const DAY_MS: i64 = 86_400_000;

/// How a title looks: the one place for names and icons.
struct TitleLook {
    id: &'static str,
    name: &'static str,
    icon_svg: &'static [u8],
}

/// Display order is the order of this table.
const TITLES: [TitleLook; 7] = [
    TitleLook {
        id: "the_carry",
        name: "The Carry",
        icon_svg: include_bytes!("../ui/icons/h2h-carry.svg"),
    },
    TitleLook {
        id: "night_owl",
        name: "Night Owl",
        icon_svg: include_bytes!("../ui/icons/h2h-night-owl.svg"),
    },
    TitleLook {
        id: "the_collector",
        name: "The Collector",
        icon_svg: include_bytes!("../ui/icons/h2h-collector.svg"),
    },
    TitleLook {
        id: "locked_in",
        name: "Locked In",
        icon_svg: include_bytes!("../ui/icons/h2h-locked-in.svg"),
    },
    TitleLook {
        id: "has_a_life",
        name: "Has a Life",
        icon_svg: include_bytes!("../ui/icons/h2h-has-a-life.svg"),
    },
    TitleLook {
        id: "sampler",
        name: "Sampler",
        icon_svg: include_bytes!("../ui/icons/h2h-sampler.svg"),
    },
    TitleLook {
        id: "one_trick",
        name: "One Trick",
        icon_svg: include_bytes!("../ui/icons/h2h-one-trick.svg"),
    },
];

/// The title ids in display order.
pub const TITLE_IDS: [&str; 7] = [
    "the_carry",
    "night_owl",
    "the_collector",
    "locked_in",
    "has_a_life",
    "sampler",
    "one_trick",
];

fn look(id: &str) -> Option<&'static TitleLook> {
    TITLES.iter().find(|t| t.id == id)
}

/// Display name of a title id, `None` for an id this client does not know.
pub fn title_name(id: &str) -> Option<&'static str> {
    look(id).map(|t| t.name)
}

/// Session titles are not on the feed card, only in the recap. They share one
/// belt icon until each gets its own.
const SESSION_TITLE_NAMES: [(&str, &str); 6] = [
    ("mouth", "Mouth"),
    ("marathon", "Marathon"),
    ("last_one_up", "Last One Up"),
    ("clipper", "Clipper"),
    ("rage_quit", "Rage Quit"),
    ("winner", "Winner"),
];

/// Name and icon of a title in the recap. A card title uses its own icon. A
/// session title, or an id this client does not know, uses the belt icon and
/// a name made from the id.
pub fn belt_look(id: &str) -> (String, slint::Image) {
    if let Some(t) = look(id) {
        return (t.name.to_string(), icon_of(id));
    }
    let name = SESSION_TITLE_NAMES
        .iter()
        .find(|(i, _)| *i == id)
        .map(|(_, n)| n.to_string())
        .unwrap_or_else(|| {
            id.split('_')
                .filter(|w| !w.is_empty())
                .map(|w| {
                    let mut c = w.chars();
                    c.next()
                        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join(" ")
        });
    let icon = slint::Image::load_from_svg_data(include_bytes!("../ui/icons/h2h-belt.svg"))
        .unwrap_or_default();
    (name, icon)
}

fn display_rank(id: &str) -> usize {
    TITLES.iter().position(|t| t.id == id).unwrap_or(usize::MAX)
}

// ---- wire ----

#[derive(Debug, Clone, Deserialize)]
struct WireTitle {
    title_id: String,
    #[serde(default)]
    holder_user_id: String,
    #[serde(default)]
    value: Value,
    #[serde(default)]
    unit: String,
    #[serde(default)]
    since_ms: i64,
    #[serde(default)]
    previous_holder_user_id: String,
    #[serde(default)]
    moved_at_ms: i64,
}

/// What the two members of a pair hold in one title they both contend for.
#[derive(Debug, Clone, Deserialize)]
struct WirePairValue {
    #[serde(default)]
    title_id: String,
    #[serde(default)]
    a_value: Value,
    #[serde(default)]
    b_value: Value,
}

#[derive(Debug, Clone, Deserialize)]
struct WirePair {
    #[serde(default)]
    a: String,
    #[serde(default)]
    b: String,
    #[serde(default)]
    a_count: i32,
    #[serde(default)]
    b_count: i32,
    // Absent from a backend that predates pair values.
    #[serde(default, deserialize_with = "null_as_empty")]
    values: Vec<WirePairValue>,
}

#[derive(Debug, Clone, Deserialize)]
struct WireFeed {
    #[serde(default)]
    member_count: i32,
    #[serde(default)]
    open_seats: i32,
    // The backend sends null for an empty list in some paths.
    #[serde(default, deserialize_with = "null_as_empty")]
    titles: Vec<WireTitle>,
    #[serde(default, deserialize_with = "null_as_empty")]
    pairs: Vec<WirePair>,
    #[serde(default)]
    belt_moved_recent: Option<WireTitle>,
    /// The viewer's own value in each title their data computes.
    #[serde(default, deserialize_with = "null_as_empty")]
    own_values: Vec<WireOwnValue>,
}

#[derive(Debug, Clone, Deserialize)]
struct WireOwnValue {
    title_id: String,
    #[serde(default)]
    value: Value,
    #[serde(default)]
    unit: String,
    /// The value as text, built by the server. Empty from an older server.
    #[serde(default)]
    display: String,
}

fn null_as_empty<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
}

// ---- model ----

/// A member as the UI shows one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    pub id: String,
    pub name: String,
    pub initials: String,
    pub hue: i32,
}

impl Person {
    pub fn new(id: &str, name: &str) -> Self {
        Person {
            id: id.to_string(),
            name: name.to_string(),
            initials: make_initials(name),
            hue: avatar_color_index(id),
        }
    }

    /// A user the roster does not know, for example one who left the crew.
    pub fn unknown(id: &str) -> Self {
        Person {
            id: id.to_string(),
            name: "Crew mate".to_string(),
            initials: "?".to_string(),
            hue: avatar_color_index(id),
        }
    }
}

/// Where the entry goes on the feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// A crew of one: the wide slot, seats and the invite inside the card.
    Wide,
    /// A belt moved in the last 24 h, two or more members: the hero slot.
    Versus,
    /// Two or more members: the wide card, two columns, with the pair rows in
    /// two columns. Under five members it carries the open seats and the red
    /// invite. From five members the invite card stands beside it.
    Pairs,
    /// A quiet feed with no hero: the belt that moved leads, two columns wide,
    /// and the pairs card is the wide card under it (frame E). A model never
    /// has this placement. The feed builder gives it to the pair of cards.
    Belt,
}

impl Placement {
    pub fn variant(self) -> &'static str {
        match self {
            Placement::Wide => "wide",
            Placement::Versus => "versus",
            Placement::Pairs => "pairs",
            Placement::Belt => "belt",
        }
    }
}

/// A title that changed hands, for the sealed chip and the centre view.
#[derive(Debug, Clone, PartialEq)]
pub struct Moved {
    pub title_id: String,
    pub moved_ms: i64,
    pub holder: Person,
    pub prev: Person,
    pub value: Value,
    pub unit: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chip {
    pub id: String,
    pub name: String,
    pub value: String,
    pub unit: String,
    pub holder_id: String,
    pub holder_hue: i32,
    pub held_days: i32,
    pub moved_recent: bool,
    /// The move behind the chip, when the chip shows the crew holder of a
    /// title that changed hands.
    pub moved: Option<Moved>,
    /// The viewer's own value, when the chip is one of a crew of one. It is
    /// sealed on a new crew (`sealed::own_reveal`).
    pub own: Option<OwnValue>,
}

/// What an own chip needs for its reveal: the value and the unit as sent.
#[derive(Debug, Clone, PartialEq)]
pub struct OwnValue {
    pub value: Value,
    pub unit: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PairRow {
    pub opponent: Person,
    pub you_count: i32,
    pub them_count: i32,
    /// The contested title that decides the pair, if any.
    pub deciding: Option<Chip>,
    /// The chip decides the pair. False when every contested title ties: the
    /// row then shows the first contested title without the emphasis.
    pub decides: bool,
    /// A belt of this pair moved in the last 24 h, after the viewer last
    /// opened the feed. The row wears the red dot and sorts first.
    pub moved_recent: bool,
}

/// The belt that moved last, for the hero of a quiet feed (frame E).
#[derive(Debug, Clone, PartialEq)]
pub struct BeltMove {
    pub title_id: String,
    pub title: String,
    /// The new holder, and the title as that holder has it.
    pub holder: Person,
    pub holder_value: String,
    /// The holder before. The value is empty when the pair does not say.
    pub prev: Person,
    pub prev_value: String,
    pub moved_ms: i64,
}

impl BeltMove {
    /// The move is news: inside the two weeks before a crew counts as dormant
    /// (`quiet_row::DORMANT_DAYS`). An older one is not the hero of a feed.
    pub fn is_news(&self, now_ms: i64) -> bool {
        now_ms - self.moved_ms < crate::quiet_row::DORMANT_DAYS * DAY_MS
    }

    /// "Sara took Night Owl from Bob".
    pub fn headline(&self) -> String {
        format!(
            "{} took {} from {}",
            self.holder.name, self.title, self.prev.name
        )
    }

    /// "03:40 beats 03:14." Empty when a value is not known.
    pub fn detail(&self) -> String {
        if self.holder_value.is_empty() || self.prev_value.is_empty() {
            String::new()
        } else {
            format!("{} beats {}.", self.holder_value, self.prev_value)
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    pub placement: Placement,
    pub member_count: i32,
    pub open_seats: i32,
    pub belt_moved_recent: bool,
    pub titles: Vec<Chip>,
    /// Wide: the viewer's titles. Versus: four, led by the moved title.
    pub chips: Vec<Chip>,
    pub pairs: Vec<PairRow>,
    /// Every pair of the viewer, drawn or not.
    pub pairs_total: usize,
    pub viewer: Person,
    /// Versus: the other member.
    pub other: Option<Person>,
    pub left_score: i32,
    pub right_score: i32,
    /// The newest belt move of any age that involves the viewer. Only a quiet
    /// feed with no hero draws it (`head2head_cards`).
    pub belt: Option<BeltMove>,
}

/// A number with thousands separators: 4167 -> "4,167".
pub(crate) fn grouped(n: i64) -> String {
    let digits = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}

/// The number and the small unit of a title: hours from 100 up are whole
/// ("4,167 h"), below that they keep one decimal ("4.6 h").
pub(crate) fn value_text(value: &Value, unit: &str) -> (String, String) {
    match value {
        Value::String(s) => (s.clone(), String::new()),
        Value::Number(n) => {
            let f = n.as_f64().unwrap_or(0.0);
            let text = if unit == "h" && f.abs() < 100.0 {
                format!("{f:.1}")
            } else {
                grouped(f.round() as i64)
            };
            (text, unit.to_string())
        }
        _ => (String::new(), unit.to_string()),
    }
}

/// Numeric size of a held value, for ranking. night_owl "HH:MM" counts
/// minutes after 18:00, the backend's day start for that title.
fn magnitude(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => {
            let mut it = s.split(':');
            let h: f64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
            let m: f64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
            ((h * 60.0 + m) - 18.0 * 60.0).rem_euclid(1440.0)
        }
        _ => 0.0,
    }
}

/// A chip for one of the viewer's own values: the viewer's hue, no holder, no
/// held days. Locked In reads "h this week" as in the mockup.
fn own_chip(o: &WireOwnValue, viewer: &Person) -> Chip {
    let (value, unit) = value_text(&o.value, &o.unit);
    let unit = if o.title_id == "locked_in" && unit == "h" {
        "h this week".to_string()
    } else {
        unit
    };
    Chip {
        id: o.title_id.clone(),
        name: title_name(&o.title_id)
            .map(str::to_string)
            .unwrap_or_else(|| o.title_id.clone()),
        value: if o.display.is_empty() {
            value
        } else {
            o.display.clone()
        },
        unit,
        holder_id: String::new(),
        holder_hue: viewer.hue,
        held_days: 0,
        moved_recent: false,
        moved: None,
        own: Some(OwnValue {
            value: o.value.clone(),
            unit: o.unit.clone(),
        }),
    }
}

fn chip_of(t: &WireTitle, roster: &dyn Fn(&str) -> Person, now_ms: i64, moved: bool) -> Chip {
    let (value, unit) = value_text(&t.value, &t.unit);
    Chip {
        id: t.title_id.clone(),
        name: title_name(&t.title_id)
            .map(str::to_string)
            .unwrap_or_else(|| t.title_id.clone()),
        value,
        unit,
        holder_id: t.holder_user_id.clone(),
        holder_hue: roster(&t.holder_user_id).hue,
        held_days: if t.since_ms > 0 && now_ms > t.since_ms {
            ((now_ms - t.since_ms) / DAY_MS) as i32
        } else {
            0
        },
        moved_recent: moved,
        moved: moved_of(t, roster),
        own: None,
    }
}

/// The move of a title, when it changed hands and this client knows the title.
fn moved_of(t: &WireTitle, roster: &dyn Fn(&str) -> Person) -> Option<Moved> {
    (t.moved_at_ms > 0
        && look(&t.title_id).is_some()
        && !t.holder_user_id.is_empty()
        && !t.previous_holder_user_id.is_empty())
    .then(|| Moved {
        title_id: t.title_id.clone(),
        moved_ms: t.moved_at_ms,
        holder: roster(&t.holder_user_id),
        prev: roster(&t.previous_holder_user_id),
        value: t.value.clone(),
        unit: t.unit.clone(),
    })
}

/// Whether the higher value holds the title. Only has_a_life goes to the
/// fewest hours.
fn higher_wins(id: &str) -> bool {
    id != "has_a_life"
}

/// The contested title that decides a pair: the one with the largest relative
/// gap, `abs(a - b) / max(a, b)`, over the titles in the pair's values. A
/// title where both values are equal, or the gap is zero, decides nothing and
/// is skipped. Ties between gaps go to the title that comes first in the
/// display order. The chip carries the value of the member who would hold the
/// title in the pair, and the held days of the crew holder when that is the
/// same member.
///
/// When every contested title ties, the row shows the first contested title
/// in the display order, and the second value of the result is `false`: the
/// chip has no deciding emphasis.
///
/// Variety: a title in `used`, already the chip of a row above, is skipped
/// when the pair has another deciding title. When every deciding title of the
/// pair is used, the largest gap wins as before.
fn deciding_chip(
    pair: &WirePair,
    titles: &[&WireTitle],
    viewer: &str,
    roster: &dyn Fn(&str) -> Person,
    now_ms: i64,
    moved_id: Option<&str>,
    used: &[String],
) -> Option<(Chip, bool)> {
    let gap_of = |v: &WirePairValue| {
        let (a, b) = (magnitude(&v.a_value), magnitude(&v.b_value));
        let top = a.max(b);
        if top > 0.0 {
            (a - b).abs() / top
        } else {
            0.0
        }
    };
    let contested: Vec<&WirePairValue> = pair
        .values
        .iter()
        .filter(|v| titles.iter().any(|t| t.title_id == v.title_id))
        .collect();
    let decisive: Vec<&WirePairValue> = contested
        .iter()
        .copied()
        .filter(|v| v.a_value != v.b_value && gap_of(v) > 0.0)
        .collect();
    let decides = !decisive.is_empty();
    let pool = if decides { &decisive } else { &contested };
    let fresh = pool.iter().any(|v| !used.contains(&v.title_id));
    let mut best: Option<(f64, &WirePairValue, &WireTitle)> = None;
    for v in pool.iter().copied() {
        if decides && fresh && used.contains(&v.title_id) {
            continue;
        }
        let Some(crew) = titles.iter().find(|t| t.title_id == v.title_id) else {
            continue;
        };
        let gap = gap_of(v);
        let better = match best {
            None => true,
            Some((bg, bv, _)) => {
                gap > bg || (gap == bg && display_rank(&v.title_id) < display_rank(&bv.title_id))
            }
        };
        if better {
            best = Some((gap, v, crew));
        }
    }
    let (_, v, crew) = best?;
    let (a, b) = (magnitude(&v.a_value), magnitude(&v.b_value));
    let a_holds = if higher_wins(&v.title_id) {
        a >= b
    } else {
        a <= b
    };
    let (holder, value) = if a_holds {
        (viewer, &v.a_value)
    } else {
        (pair.b.as_str(), &v.b_value)
    };
    let mut chip = chip_of(
        &WireTitle {
            value: value.clone(),
            holder_user_id: holder.to_string(),
            ..(*crew).clone()
        },
        roster,
        now_ms,
        moved_id == Some(crew.title_id.as_str()),
    );
    if crew.holder_user_id != holder {
        chip.held_days = 0;
        chip.moved_recent = false;
        chip.moved = None;
    }
    Some((chip, decides))
}

/// A held value as text: "03:40", "4,201 h".
fn value_line(v: &Value, unit: &str) -> String {
    let (text, unit) = value_text(v, unit);
    if unit.is_empty() || text.is_empty() {
        text
    } else {
        format!("{text} {unit}")
    }
}

/// The newest title that changed hands, of any age, when the viewer held it
/// before or holds it now. A move between two other members has no hero.
fn belt_move(
    wire: &WireFeed,
    viewer: &Person,
    roster: &dyn Fn(&str) -> Person,
) -> Option<BeltMove> {
    let t = wire
        .titles
        .iter()
        .filter(|t| {
            look(&t.title_id).is_some()
                && t.moved_at_ms > 0
                && !t.holder_user_id.is_empty()
                && !t.previous_holder_user_id.is_empty()
        })
        .max_by_key(|t| t.moved_at_ms)?;
    let viewer_holds = t.holder_user_id == viewer.id;
    if !viewer_holds && t.previous_holder_user_id != viewer.id {
        return None;
    }
    let other_id = if viewer_holds {
        &t.previous_holder_user_id
    } else {
        &t.holder_user_id
    };
    // The pair says what both members have in the title, so the old holder's
    // number is known when the viewer is one of the two.
    let (viewer_v, other_v) = wire
        .pairs
        .iter()
        .find(|p| p.a == viewer.id && &p.b == other_id)
        .and_then(|p| p.values.iter().find(|v| v.title_id == t.title_id))
        .map(|v| {
            (
                value_line(&v.a_value, &t.unit),
                value_line(&v.b_value, &t.unit),
            )
        })
        .unwrap_or_default();
    let prev_value = if viewer_holds { other_v } else { viewer_v };
    Some(BeltMove {
        title_id: t.title_id.clone(),
        title: title_name(&t.title_id).unwrap_or_default().to_string(),
        holder: roster(&t.holder_user_id),
        holder_value: value_line(&t.value, &t.unit),
        prev: roster(&t.previous_holder_user_id),
        prev_value,
        moved_ms: t.moved_at_ms,
    })
}

/// Now, in ms since the epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Whether a belt held by one of the two members moved in the last 24 h and
/// after `seen_ms`, the last time the viewer opened the crew feed.
/// A belt the user opened no longer counts: opening it clears the dot.
fn moved_recent(
    titles: &[WireTitle],
    a: &str,
    b: &str,
    now_ms: i64,
    seen_ms: i64,
    opened: &dyn Fn(&str) -> bool,
) -> bool {
    titles.iter().any(|t| {
        (t.holder_user_id == a || t.holder_user_id == b)
            && t.moved_at_ms > 0
            && t.moved_at_ms > seen_ms
            && now_ms - t.moved_at_ms < DAY_MS
            && !opened(&crate::sealed::belt_key(&t.title_id, t.moved_at_ms))
    })
}

/// Build the model with nothing marked unread.
#[cfg(test)]
pub fn build(
    data: &Value,
    viewer: &Person,
    roster: &dyn Fn(&str) -> Person,
    now_ms: i64,
) -> Option<Model> {
    build_seen(data, viewer, roster, now_ms, i64::MAX)
}

/// Build the model from the entry data. `None` when the data does not parse.
/// A move after `seen_ms` (the viewer's last feed open for this crew) is
/// unread and marks its pair row.
#[cfg(test)]
pub fn build_seen(
    data: &Value,
    viewer: &Person,
    roster: &dyn Fn(&str) -> Person,
    now_ms: i64,
    seen_ms: i64,
) -> Option<Model> {
    build_rows_sealed(data, viewer, roster, now_ms, seen_ms, &|_| false)
}

/// `build_seen` that knows which belts the user opened (`opened` takes the
/// key of a move). An opened belt does not mark its pair row unread.
pub fn build_rows_sealed(
    data: &Value,
    viewer: &Person,
    roster: &dyn Fn(&str) -> Person,
    now_ms: i64,
    seen_ms: i64,
    opened: &dyn Fn(&str) -> bool,
) -> Option<Model> {
    let wire: WireFeed = serde_json::from_value(data.clone()).ok()?;
    let known: Vec<&WireTitle> = wire
        .titles
        .iter()
        .filter(|t| look(&t.title_id).is_some() && !t.holder_user_id.is_empty())
        .collect();
    let mut sorted: Vec<&WireTitle> = known.clone();
    sorted.sort_by_key(|t| display_rank(&t.title_id));
    let moved_id = wire.belt_moved_recent.as_ref().map(|t| t.title_id.clone());
    let titles: Vec<Chip> = sorted
        .iter()
        .map(|t| chip_of(t, roster, now_ms, moved_id.as_deref() == Some(&t.title_id)))
        .collect();

    // The viewer is A in every pair. Rows keep the payload order. A pair with
    // no contested title (0:0, the two have nothing in common yet) is not a
    // row: it would sort as the closest tally of all and lead the card with
    // nothing to say.
    let mut pairs: Vec<(PairRow, &WirePair)> = wire
        .pairs
        .iter()
        .filter(|p| p.a == viewer.id && p.a_count + p.b_count > 0)
        .map(|p| {
            (
                PairRow {
                    opponent: roster(&p.b),
                    you_count: p.a_count,
                    them_count: p.b_count,
                    deciding: None,
                    decides: false,
                    moved_recent: moved_recent(
                        &wire.titles,
                        &viewer.id,
                        &p.b,
                        now_ms,
                        seen_ms,
                        opened,
                    ),
                },
                p,
            )
        })
        .collect();

    let belt_moved = wire.belt_moved_recent.is_some();
    let mut model = Model {
        placement: Placement::Pairs,
        member_count: wire.member_count,
        open_seats: wire.open_seats.clamp(0, SEATS),
        belt_moved_recent: belt_moved,
        titles,
        chips: Vec::new(),
        pairs: Vec::new(),
        pairs_total: pairs.len(),
        viewer: viewer.clone(),
        other: None,
        left_score: 0,
        right_score: 0,
        belt: belt_move(&wire, viewer, roster),
    };

    if wire.member_count < 2 {
        model.placement = Placement::Wide;
        model.open_seats = (SEATS - wire.member_count.max(1)).clamp(0, SEATS);
        // The viewer's own values, first four in display order. An older
        // server sends none: then the titles the viewer holds stand in.
        let mut own: Vec<&WireOwnValue> = wire
            .own_values
            .iter()
            .filter(|o| look(&o.title_id).is_some())
            .collect();
        own.sort_by_key(|o| display_rank(&o.title_id));
        model.chips = if own.is_empty() {
            model
                .titles
                .iter()
                .filter(|c| c.holder_id == viewer.id)
                .take(CHIPS)
                .cloned()
                .collect()
        } else {
            own.iter()
                .take(CHIPS)
                .map(|o| own_chip(o, viewer))
                .collect()
        };
        return Some(model);
    }

    if let Some(moved) = &wire.belt_moved_recent {
        // Viewer against the new holder; when the viewer is the new holder,
        // against the member who lost the title.
        let involves_viewer =
            moved.holder_user_id == viewer.id || moved.previous_holder_user_id == viewer.id;
        let other_id = if !involves_viewer {
            // Not the viewer's belt: no hero. The cell carries it as a dot.
            None
        } else if moved.holder_user_id != viewer.id {
            Some(moved.holder_user_id.clone())
        } else if !moved.previous_holder_user_id.is_empty() {
            Some(moved.previous_holder_user_id.clone())
        } else {
            pairs.first().map(|p| p.0.opponent.id.clone())
        };
        let pair = other_id
            .as_ref()
            .and_then(|o| wire.pairs.iter().find(|p| p.a == viewer.id && &p.b == o));
        if let (Some(other_id), Some(pair)) = (other_id, pair) {
            model.placement = Placement::Versus;
            model.other = Some(roster(&other_id));
            model.left_score = pair.a_count;
            model.right_score = pair.b_count;
            let moved_chip = chip_of(moved, roster, now_ms, true);
            let mut chips = vec![moved_chip];
            chips.extend(
                model
                    .titles
                    .iter()
                    .filter(|c| c.id != moved.title_id)
                    .cloned(),
            );
            chips.truncate(CHIPS);
            model.chips = chips;
            return Some(model);
        }
    }

    // Closest tallies first: the pairs that are still open to a move lead.
    // The sort is stable, so equal gaps keep the payload order.
    pairs.sort_by_key(|(p, _)| (!p.moved_recent, (p.you_count - p.them_count).abs()));
    if wire.member_count >= SEATS {
        model.open_seats = 0;
    }
    pairs.truncate(PAIR_ROWS);
    // The chips are picked row by row, top down, so a title that decided a
    // higher row gives way to another contested title in the rows below.
    let mut used: Vec<String> = Vec::new();
    for (mut row, wire_pair) in pairs {
        let picked = deciding_chip(
            wire_pair,
            &known,
            &viewer.id,
            roster,
            now_ms,
            moved_id.as_deref(),
            &used,
        );
        row.decides = picked.as_ref().is_some_and(|(_, d)| *d);
        row.deciding = picked.map(|(c, _)| c);
        if let (Some(c), true) = (&row.deciding, row.decides) {
            used.push(c.id.clone());
        }
        model.pairs.push(row);
    }
    Some(model)
}

// ---- Slint ----

fn icon_of(id: &str) -> slint::Image {
    // A rebuild draws every chip again, and parsing an SVG is the slow part.
    thread_local! {
        static ICONS: std::cell::RefCell<std::collections::HashMap<String, slint::Image>> =
            std::cell::RefCell::new(std::collections::HashMap::new());
    }
    ICONS.with(|cache| {
        cache
            .borrow_mut()
            .entry(id.to_string())
            .or_insert_with(|| {
                look(id)
                    .and_then(|t| slint::Image::load_from_svg_data(t.icon_svg).ok())
                    .unwrap_or_default()
            })
            .clone()
    })
}

/// The small label under a chip: "held 9 days", "held 1 day", and nothing
/// under one day.
pub fn held_label(days: i32) -> String {
    match days {
        d if d < 1 => String::new(),
        1 => "held 1 day".to_string(),
        d => format!("held {d} days"),
    }
}

fn title_held(
    c: &Chip,
    viewer: &Person,
    avatar: &dyn Fn(&str) -> Option<slint::Image>,
    seal: &Seal,
) -> TitleHeld {
    TitleHeld {
        belt: c
            .own
            .as_ref()
            .map(|o| {
                crate::sealed::own_reveal(
                    &crate::sealed::OwnValue {
                        title_id: &c.id,
                        viewer,
                        value: &o.value,
                        unit: &o.unit,
                        shown_unit: &c.unit,
                    },
                    avatar,
                    seal,
                )
            })
            .or_else(|| {
                c.moved.as_ref().map(|m| {
                    crate::sealed::reveal_of(
                        &crate::sealed::Move {
                            title_id: &m.title_id,
                            moved_ms: m.moved_ms,
                            holder: &m.holder,
                            prev: &m.prev,
                            value: &m.value,
                            unit: &m.unit,
                        },
                        avatar,
                        seal,
                    )
                })
            })
            .unwrap_or_default(),
        id: c.id.as_str().into(),
        icon: icon_of(&c.id),
        title: c.name.as_str().into(),
        value: c.value.as_str().into(),
        unit: c.unit.as_str().into(),
        hue_index: c.holder_hue,
        holder_id: c.holder_id.as_str().into(),
        held_days: c.held_days,
        held_label: held_label(c.held_days).as_str().into(),
        moved_recent: c.moved_recent,
    }
}

fn model_of<T: Clone + 'static>(items: Vec<T>) -> slint::ModelRc<T> {
    slint::ModelRc::new(slint::VecModel::from(items))
}

/// The UI struct of the head2head card, with nothing sealed.
#[cfg(test)]
pub fn to_slint(m: &Model, avatar: &dyn Fn(&str) -> Option<slint::Image>) -> Head2headData {
    to_slint_sealed(m, avatar, &Seal::off())
}

/// The UI struct of the head2head card. A chip of a belt that moved and is not
/// opened is sealed (`seal`).
pub fn to_slint_sealed(
    m: &Model,
    avatar: &dyn Fn(&str) -> Option<slint::Image>,
    seal: &Seal,
) -> Head2headData {
    let pic = |id: &str| avatar(id).unwrap_or_default();
    let has = |id: &str| avatar(id).is_some();
    let pairs: Vec<H2hPairRow> = m
        .pairs
        .iter()
        .map(|p| H2hPairRow {
            id: p.opponent.id.as_str().into(),
            name: p.opponent.name.as_str().into(),
            initials: p.opponent.initials.as_str().into(),
            hue_index: p.opponent.hue,
            avatar: pic(&p.opponent.id),
            has_avatar: has(&p.opponent.id),
            you_initials: m.viewer.initials.as_str().into(),
            you_hue_index: m.viewer.hue,
            you_avatar: pic(&m.viewer.id),
            you_has_avatar: has(&m.viewer.id),
            you_count: p.you_count,
            them_count: p.them_count,
            has_chip: p.deciding.is_some(),
            chip_ties: p.deciding.is_some() && !p.decides,
            moved_recent: p.moved_recent,
            chip: p
                .deciding
                .as_ref()
                .map(|c| title_held(c, &m.viewer, avatar, seal))
                .unwrap_or_default(),
        })
        .collect();
    let (right_id, right_name, right_initials, right_hue) = m
        .other
        .as_ref()
        .map(|o| (o.id.as_str(), o.name.as_str(), o.initials.as_str(), o.hue))
        .unwrap_or(("", "", "", 0));
    Head2headData {
        variant: m.placement.variant().into(),
        member_count: m.member_count,
        open_seats: m.open_seats,
        belt_moved_recent: m.belt_moved_recent,
        titles: model_of(
            m.titles
                .iter()
                .map(|c| title_held(c, &m.viewer, avatar, seal))
                .collect(),
        ),
        chips: model_of(
            m.chips
                .iter()
                .map(|c| title_held(c, &m.viewer, avatar, seal))
                .collect(),
        ),
        pairs: model_of(pairs),
        left_id: m.viewer.id.as_str().into(),
        left_name: m.viewer.name.as_str().into(),
        left_initials: m.viewer.initials.as_str().into(),
        left_hue_index: m.viewer.hue,
        left_avatar: pic(&m.viewer.id),
        left_has_avatar: has(&m.viewer.id),
        right_id: right_id.into(),
        right_name: right_name.into(),
        right_initials: right_initials.into(),
        right_hue_index: right_hue,
        right_avatar: pic(right_id),
        right_has_avatar: !right_id.is_empty() && has(right_id),
        pairs_total: m.pairs_total as i32,
        // Only the pairs card has a pair list to continue.
        more_pairs: match m.placement {
            Placement::Pairs => m.pairs_total.saturating_sub(m.pairs.len()) as i32,
            _ => 0,
        },
        left_score: m.left_score,
        right_score: m.right_score,
        members: model_of(Vec::new()),
        more_members: 0,
        belt: Default::default(),
    }
}

/// The UI struct of the belt hero (frame E), or `None` without a move.
pub fn belt_hero_data(
    m: &Model,
    now_ms: i64,
    avatar: &dyn Fn(&str) -> Option<slint::Image>,
) -> Option<Head2headData> {
    let b = m.belt.as_ref().filter(|b| b.is_news(now_ms))?;
    let person = |p: &Person| H2hPerson {
        id: p.id.as_str().into(),
        name: p.name.as_str().into(),
        initials: p.initials.as_str().into(),
        hue_index: p.hue,
        avatar: avatar(&p.id).unwrap_or_default(),
        has_avatar: avatar(&p.id).is_some(),
    };
    Some(Head2headData {
        variant: Placement::Belt.variant().into(),
        member_count: m.member_count,
        belt: crate::BeltHeroData {
            title_id: b.title_id.as_str().into(),
            title: b.title.as_str().into(),
            icon: icon_of(&b.title_id),
            hue_index: b.holder.hue,
            prev: person(&b.prev),
            prev_value: b.prev_value.as_str().into(),
            holder: person(&b.holder),
            holder_value: b.holder_value.as_str().into(),
            age: crate::feed_age::ago_label(b.moved_ms, now_ms).into(),
            headline: b.headline().into(),
            detail: b.detail().into(),
        },
        ..Default::default()
    })
}

/// The UI struct of the invite card: the first members as octagons.
pub fn invite_card_data(
    members: &[Person],
    member_count: i32,
    avatar: &dyn Fn(&str) -> Option<slint::Image>,
) -> Head2headData {
    let shown: Vec<H2hPerson> = members
        .iter()
        .take(INVITE_CARD_MEMBERS)
        .map(|p| H2hPerson {
            id: p.id.as_str().into(),
            name: p.name.as_str().into(),
            initials: p.initials.as_str().into(),
            hue_index: p.hue,
            avatar: avatar(&p.id).unwrap_or_default(),
            has_avatar: avatar(&p.id).is_some(),
        })
        .collect();
    let more = (member_count - shown.len() as i32).max(0);
    Head2headData {
        variant: "invite".into(),
        member_count,
        members: model_of(shown),
        more_members: more,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1_760_000_000_000;

    fn me() -> Person {
        Person::new("me", "Bob")
    }

    fn roster(id: &str) -> Person {
        match id {
            "me" => me(),
            "u2" => Person::new("u2", "Jonas"),
            "u3" => Person::new("u3", "Max"),
            other => Person::unknown(other),
        }
    }

    fn title(id: &str, holder: &str, value: Value, unit: &str) -> Value {
        json!({"title_id": id, "holder_user_id": holder, "value": value, "unit": unit, "since_ms": NOW - 3 * DAY_MS})
    }

    fn pair(b: &str, a: i32, bc: i32) -> Value {
        json!({"a": "me", "b": b, "a_count": a, "b_count": bc})
    }

    fn feed(members: i32, titles: Vec<Value>, pairs: Vec<Value>, moved: Option<Value>) -> Value {
        let mut v = json!({
            "crew_id": "c", "member_count": members, "open_seats": (5 - members).max(0),
            "titles": titles, "pairs": pairs, "updated_at": NOW,
        });
        if let Some(m) = moved {
            v["belt_moved_recent"] = m;
        }
        v
    }

    #[test]
    fn title_display_map_covers_the_seven_titles() {
        let rows: Vec<_> = TITLES.iter().map(|t| (t.id, t.name)).collect();
        assert_eq!(
            rows,
            vec![
                ("the_carry", "The Carry"),
                ("night_owl", "Night Owl"),
                ("the_collector", "The Collector"),
                ("locked_in", "Locked In"),
                ("has_a_life", "Has a Life"),
                ("sampler", "Sampler"),
                ("one_trick", "One Trick"),
            ]
        );
        for t in &TITLES {
            assert!(
                slint::Image::load_from_svg_data(t.icon_svg).is_ok(),
                "{} icon must parse",
                t.id
            );
        }
    }

    #[test]
    fn the_held_label_reads_like_the_mockup() {
        assert_eq!(held_label(0), "");
        assert_eq!(held_label(-3), "");
        assert_eq!(held_label(1), "held 1 day");
        assert_eq!(held_label(9), "held 9 days");
        // Frame F: a dormant crew, 24 days on every belt.
        assert_eq!(held_label(24), "held 24 days");
    }

    #[test]
    fn values_read_like_the_mockup() {
        assert_eq!(
            value_text(&json!(4167.0), "h"),
            ("4,167".into(), "h".into())
        );
        assert_eq!(value_text(&json!(4.6), "h"), ("4.6".into(), "h".into()));
        assert_eq!(value_text(&json!(6.0), "h"), ("6.0".into(), "h".into()));
        assert_eq!(
            value_text(&json!(14), "installed"),
            ("14".into(), "installed".into())
        );
        assert_eq!(value_text(&json!(80), "%"), ("80".into(), "%".into()));
        assert_eq!(value_text(&json!("03:14"), ""), ("03:14".into(), "".into()));
    }

    #[test]
    fn a_crew_of_one_is_the_wide_card_with_the_viewers_titles() {
        let data = feed(
            1,
            vec![
                title("the_carry", "me", json!(4167.0), "h"),
                title("night_owl", "me", json!("03:14"), ""),
            ],
            vec![],
            None,
        );
        let m = build(&data, &me(), &roster, NOW).unwrap();
        assert_eq!(m.placement, Placement::Wide);
        assert_eq!(m.open_seats, 4);
        let ids: Vec<_> = m.chips.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["the_carry", "night_owl"]);
        assert_eq!(m.chips[0].held_days, 3);
    }

    fn own(id: &str, value: Value, unit: &str, display: &str) -> Value {
        json!({"title_id": id, "value": value, "unit": unit, "display": display})
    }

    #[test]
    fn a_solo_crew_shows_the_viewers_own_values_in_display_order() {
        let mut data = feed(1, vec![], vec![], None);
        data["own_values"] = json!([
            own("the_carry", json!(4167.0), "h", "4,167"),
            own("night_owl", json!("03:14"), "", "03:14"),
            own("the_collector", json!(5), "installed", "5"),
            own("locked_in", json!(4.6), "h", "4.6"),
            own("has_a_life", json!(4.6), "h", "4.6"),
            own("mouth", json!(30), "min", "30"),
        ]);
        // Out of order on the wire: the card sorts by display order.
        data["own_values"].as_array_mut().unwrap().reverse();
        let m = build(&data, &me(), &roster, NOW).unwrap();
        assert_eq!(m.placement, Placement::Wide);
        let ids: Vec<_> = m.chips.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            ["the_carry", "night_owl", "the_collector", "locked_in"]
        );
        assert_eq!(m.chips[0].value, "4,167");
        assert_eq!(m.chips[0].unit, "h");
        assert_eq!(m.chips[3].value, "4.6");
        assert_eq!(m.chips[3].unit, "h this week");
        for c in &m.chips {
            assert_eq!(c.holder_hue, me().hue, "the viewer's own hue");
            assert_eq!(c.held_days, 0);
        }
    }

    #[test]
    fn own_values_fall_back_to_value_text_without_a_display() {
        let mut data = feed(1, vec![], vec![], None);
        data["own_values"] = json!([{"title_id": "the_carry", "value": 4167.0, "unit": "h"}]);
        let m = build(&data, &me(), &roster, NOW).unwrap();
        assert_eq!(m.chips[0].value, "4,167");
    }

    #[test]
    fn two_members_without_a_move_are_the_pairs_card_with_one_pair() {
        let data = feed(
            2,
            vec![
                title("the_carry", "me", json!(4167.0), "h"),
                title("the_collector", "u2", json!(14), "installed"),
            ],
            vec![pair("u2", 1, 1)],
            None,
        );
        let m = build(&data, &me(), &roster, NOW).unwrap();
        assert_eq!(m.placement, Placement::Pairs);
        assert_eq!(m.open_seats, 3);
        assert_eq!(m.pairs.len(), 1);
        assert_eq!(m.pairs[0].opponent.name, "Jonas");
        assert_eq!((m.pairs[0].you_count, m.pairs[0].them_count), (1, 1));
    }

    #[test]
    fn a_recent_move_makes_the_versus_hero_led_by_the_moved_title() {
        let moved = json!({"title_id": "the_collector", "holder_user_id": "u2", "value": 14,
            "unit": "installed", "since_ms": NOW - 1000, "moved_at_ms": NOW - 1000,
            "previous_holder_user_id": "me"});
        let data = feed(
            2,
            vec![
                title("the_carry", "me", json!(4167.0), "h"),
                title("night_owl", "me", json!("03:14"), ""),
                moved.clone(),
                title("locked_in", "me", json!(4.6), "h"),
                title("sampler", "u2", json!(5), "games"),
            ],
            vec![pair("u2", 3, 2)],
            Some(moved),
        );
        let m = build(&data, &me(), &roster, NOW).unwrap();
        assert_eq!(m.placement, Placement::Versus);
        assert_eq!(m.other.as_ref().unwrap().name, "Jonas");
        assert_eq!((m.left_score, m.right_score), (3, 2));
        assert_eq!(m.chips.len(), 4);
        assert_eq!(m.chips[0].id, "the_collector");
        assert!(m.chips[0].moved_recent);
        assert_eq!(m.chips[0].holder_hue, roster("u2").hue);
    }

    #[test]
    fn when_the_viewer_took_the_title_the_versus_is_against_the_loser() {
        let moved = json!({"title_id": "the_carry", "holder_user_id": "me", "value": 9.5,
            "unit": "h", "since_ms": NOW - 1000, "moved_at_ms": NOW - 1000,
            "previous_holder_user_id": "u3"});
        let data = feed(
            3,
            vec![moved.clone()],
            vec![pair("u2", 1, 0), pair("u3", 2, 1)],
            Some(moved),
        );
        let m = build(&data, &me(), &roster, NOW).unwrap();
        assert_eq!(m.placement, Placement::Versus);
        assert_eq!(m.other.unwrap().id, "u3");
        assert_eq!((m.left_score, m.right_score), (2, 1));
    }

    #[test]
    fn five_members_drop_the_seats() {
        let data = feed(
            5,
            vec![title("the_carry", "me", json!(4167.0), "h")],
            vec![
                pair("u2", 1, 1),
                pair("u3", 1, 1),
                pair("u4", 1, 1),
                pair("u5", 1, 1),
            ],
            None,
        );
        let m = build(&data, &me(), &roster, NOW).unwrap();
        assert_eq!(m.placement, Placement::Pairs);
        assert_eq!(m.open_seats, 0);
        assert_eq!(m.pairs.len(), 4);
    }

    /// Frame D: five members, four pairs, all four on the card in two columns.
    /// No "+N more" line, because no row remains. Four members have three pairs
    /// and seats; seven members have six pairs, all shown; eight have seven, so
    /// one remains.
    #[test]
    fn five_members_show_four_pairs_and_no_more_line() {
        let roster = |id: &str| Person::new(id, id);
        let pairs = vec![
            pair("u2", 1, 1),
            pair("u3", 2, 0),
            pair("u4", 0, 2),
            pair("u5", 3, 1),
        ];
        let data = feed(5, vec![], pairs.clone(), None);
        let m = build(&data, &me(), &roster, NOW).unwrap();
        assert_eq!(m.placement, Placement::Pairs);
        assert_eq!(m.pairs.len(), 4);
        assert_eq!(to_slint(&m, &|_| None).more_pairs, 0);

        let data = feed(4, vec![], pairs[..3].to_vec(), None);
        let m = build(&data, &me(), &roster, NOW).unwrap();
        assert_eq!(m.placement, Placement::Pairs, "always two columns wide");
        assert_eq!(m.pairs.len(), 3, "every pair fits with seats");
        assert_eq!(m.open_seats, 1);
        assert_eq!(to_slint(&m, &|_| None).more_pairs, 0);

        let mut six = pairs;
        six.extend([pair("u6", 1, 1), pair("u7", 1, 1)]);
        let data = feed(7, vec![], six.clone(), None);
        let m = build(&data, &me(), &roster, NOW).unwrap();
        assert_eq!(m.pairs.len(), 6, "three rows in each column");
        assert_eq!(to_slint(&m, &|_| None).more_pairs, 0);

        six.push(pair("u8", 1, 1));
        let data = feed(8, vec![], six, None);
        let m = build(&data, &me(), &roster, NOW).unwrap();
        assert_eq!(m.pairs.len(), 6);
        assert_eq!(to_slint(&m, &|_| None).more_pairs, 1);
    }

    fn pair_v(b: &str, a: i32, bc: i32, values: Vec<Value>) -> Value {
        json!({"a": "me", "b": b, "a_count": a, "b_count": bc, "values": values})
    }

    fn pv(id: &str, a: Value, b: Value) -> Value {
        json!({"title_id": id, "a_value": a, "b_value": b})
    }

    #[test]
    fn the_deciding_chip_is_the_title_with_the_largest_real_gap() {
        // The crew holder of the_carry is u3, not in the pair. The pair gap is
        // 4171 against 4.6 hours: the widest. locked_in is 4.6 against 4.4.
        let data = feed(
            3,
            vec![
                title("locked_in", "me", json!(4.6), "h"),
                title("the_carry", "u3", json!(9000.0), "h"),
                title("sampler", "u2", json!(9), "games"),
            ],
            vec![pair_v(
                "u2",
                1,
                2,
                vec![
                    pv("locked_in", json!(4.6), json!(4.4)),
                    pv("the_carry", json!(4.6), json!(4171.0)),
                    pv("sampler", json!(3), json!(9)),
                ],
            )],
            None,
        );
        let m = build(&data, &me(), &roster, NOW).unwrap();
        let chip = m.pairs[0].deciding.as_ref().unwrap();
        assert_eq!(chip.id, "the_carry");
        assert_eq!(chip.holder_id, "u2", "u2 holds it in the pair");
        assert_eq!(chip.value, "4,171");
        assert_eq!(chip.held_days, 0, "the crew holder is u3, so no held days");
    }

    #[test]
    fn a_tied_title_never_decides_while_another_title_has_a_gap() {
        // One Trick is 100 against 100: a tie. The Collector has a gap, and
        // wins although One Trick comes later in the display order.
        let data = feed(
            2,
            vec![
                title("one_trick", "me", json!(100), "%"),
                title("the_collector", "me", json!(12), "installed"),
            ],
            vec![pair_v(
                "u2",
                1,
                1,
                vec![
                    pv("one_trick", json!(100), json!(100)),
                    pv("the_collector", json!(12), json!(10)),
                ],
            )],
            None,
        );
        let m = build(&data, &me(), &roster, NOW).unwrap();
        assert_eq!(m.pairs[0].deciding.as_ref().unwrap().id, "the_collector");
        assert!(m.pairs[0].decides);
        use slint::Model;
        let d = to_slint(&m, &|_| None);
        assert!(!d.pairs.row_data(0).unwrap().chip_ties);
    }

    #[test]
    fn when_every_contested_title_ties_the_row_shows_the_first_without_emphasis() {
        let data = feed(
            2,
            vec![
                title("one_trick", "me", json!(100), "%"),
                title("the_collector", "me", json!(10), "installed"),
            ],
            vec![pair_v(
                "u2",
                1,
                1,
                vec![
                    pv("one_trick", json!(100), json!(100)),
                    pv("the_collector", json!(10), json!(10)),
                ],
            )],
            None,
        );
        let m = build(&data, &me(), &roster, NOW).unwrap();
        let row = &m.pairs[0];
        assert_eq!(row.deciding.as_ref().unwrap().id, "the_collector");
        assert!(!row.decides, "no deciding emphasis");
        use slint::Model;
        let d = to_slint(&m, &|_| None);
        let r = d.pairs.row_data(0).unwrap();
        assert!(r.has_chip && r.chip_ties);
    }

    #[test]
    fn the_gap_is_relative_not_absolute() {
        // 40 games against 20 is a 50% gap. 9.0 h against 3.0 h is 67%.
        let data = feed(
            2,
            vec![
                title("the_collector", "me", json!(40), "installed"),
                title("locked_in", "u2", json!(9.0), "h"),
            ],
            vec![pair_v(
                "u2",
                1,
                1,
                vec![
                    pv("the_collector", json!(40), json!(20)),
                    pv("locked_in", json!(3.0), json!(9.0)),
                ],
            )],
            None,
        );
        let m = build(&data, &me(), &roster, NOW).unwrap();
        assert_eq!(m.pairs[0].deciding.as_ref().unwrap().id, "locked_in");
    }

    #[test]
    fn equal_gaps_go_to_the_title_first_in_display_order() {
        let data = feed(
            2,
            vec![
                title("locked_in", "me", json!(8.0), "h"),
                title("the_collector", "me", json!(10), "installed"),
            ],
            vec![pair_v(
                "u2",
                2,
                0,
                vec![
                    pv("locked_in", json!(8.0), json!(4.0)),
                    pv("the_collector", json!(10), json!(5)),
                ],
            )],
            None,
        );
        let m = build(&data, &me(), &roster, NOW).unwrap();
        // the_collector comes before locked_in in the table.
        assert_eq!(m.pairs[0].deciding.as_ref().unwrap().id, "the_collector");
    }

    #[test]
    fn a_title_that_decided_a_higher_row_is_skipped_below() {
        // The Collector (6 against 1) has the widest gap in all three pairs.
        // Row one takes it. Row two takes the next widest, Locked In. Row
        // three has only the_collector and locked_in left to take, both used,
        // so the widest gap wins again.
        let vals = |c: (i32, i32), l: (f64, f64)| {
            vec![
                pv("the_collector", json!(c.0), json!(c.1)),
                pv("locked_in", json!(l.0), json!(l.1)),
            ]
        };
        let data = feed(
            5,
            vec![
                title("the_collector", "me", json!(6), "installed"),
                title("locked_in", "me", json!(9.0), "h"),
            ],
            vec![
                pair_v("u2", 1, 1, vals((6, 1), (9.0, 6.0))),
                pair_v("u3", 1, 1, vals((6, 1), (9.0, 6.0))),
                pair_v("u4", 1, 1, vals((6, 1), (9.0, 6.0))),
            ],
            None,
        );
        let m = build(&data, &me(), &roster, NOW).unwrap();
        let ids: Vec<&str> = m
            .pairs
            .iter()
            .map(|p| p.deciding.as_ref().unwrap().id.as_str())
            .collect();
        assert_eq!(ids, ["the_collector", "locked_in", "the_collector"]);
    }

    #[test]
    fn has_a_life_goes_to_the_fewer_hours_and_keeps_held_days_for_the_holder() {
        let data = feed(
            2,
            vec![title("has_a_life", "me", json!(2.0), "h")],
            vec![pair_v(
                "u2",
                1,
                0,
                vec![pv("has_a_life", json!(2.0), json!(30.0))],
            )],
            None,
        );
        let m = build(&data, &me(), &roster, NOW).unwrap();
        let chip = m.pairs[0].deciding.as_ref().unwrap();
        assert_eq!(
            (chip.holder_id.as_str(), chip.value.as_str()),
            ("me", "2.0")
        );
        assert_eq!(chip.held_days, 3);
    }

    #[test]
    fn night_owl_gap_counts_minutes_after_six_pm() {
        // 01:00 is 420 min after 18:00, 23:00 is 300: a 29% gap. A plain
        // clock compare would call 01:00 the smaller value.
        let data = feed(
            2,
            vec![
                title("night_owl", "u2", json!("01:00"), ""),
                title("the_collector", "me", json!(10), "installed"),
            ],
            vec![pair_v(
                "u2",
                1,
                1,
                vec![
                    pv("night_owl", json!("23:00"), json!("01:00")),
                    pv("the_collector", json!(10), json!(9)),
                ],
            )],
            None,
        );
        let m = build(&data, &me(), &roster, NOW).unwrap();
        let chip = m.pairs[0].deciding.as_ref().unwrap();
        assert_eq!(
            (chip.id.as_str(), chip.holder_id.as_str()),
            ("night_owl", "u2")
        );
        assert_eq!(chip.value, "01:00");
    }

    #[test]
    fn a_pair_without_a_contested_title_is_not_a_row() {
        let data = feed(
            3,
            vec![title("the_carry", "u3", json!(9000.0), "h")],
            vec![pair("u2", 0, 0)],
            None,
        );
        let m = build(&data, &me(), &roster, NOW).unwrap();
        assert!(m.pairs.is_empty() && m.pairs_total == 0);
        let data = json!({"member_count": 2, "open_seats": 3, "titles": [],
            "pairs": [{"a": "me", "b": "u2", "a_count": 0, "b_count": 0, "values": null}]});
        let m = build(&data, &me(), &roster, NOW).unwrap();
        assert!(m.pairs.is_empty());
    }

    #[test]
    fn empty_pairs_never_lead_the_card() {
        // Production on day 3 of a crew of ten: nine pairs, one contested.
        // The empty ones used to sort first (gap 0) and push the real pair to
        // the last row.
        let mut pairs: Vec<Value> = (2..10).map(|i| pair(&format!("u{i}"), 0, 0)).collect();
        pairs.push(pair("u10", 0, 2));
        let data = feed(10, vec![], pairs, None);
        let m = build(&data, &me(), &roster, NOW).unwrap();
        let order: Vec<&str> = m.pairs.iter().map(|p| p.opponent.id.as_str()).collect();
        assert_eq!(order, ["u10"]);
        assert_eq!((m.pairs_total, to_slint(&m, &|_| None).more_pairs), (1, 0));
    }

    #[test]
    fn unknown_title_ids_and_null_lists_do_not_break_the_entry() {
        let data = json!({"member_count": 2, "open_seats": 3, "titles": null, "pairs": null});
        let m = build(&data, &me(), &roster, NOW).unwrap();
        assert!(m.titles.is_empty() && m.pairs.is_empty());
        let data = feed(
            2,
            vec![title("brand_new", "u2", json!(1), "")],
            vec![pair("u2", 0, 0)],
            None,
        );
        let m = build(&data, &me(), &roster, NOW).unwrap();
        assert!(m.titles.is_empty());
        assert!(m.pairs.is_empty());
    }

    #[test]
    fn the_invite_card_shows_eight_members_then_a_count() {
        let members: Vec<Person> = (0..11)
            .map(|i| Person::new(&format!("u{i}"), "Kim"))
            .collect();
        let d = invite_card_data(&members, 11, &|_| None);
        use slint::Model;
        assert_eq!(d.members.row_count(), 8);
        assert_eq!(d.more_members, 3);
        let d = invite_card_data(&members[..5], 5, &|_| None);
        assert_eq!((d.members.row_count(), d.more_members), (5, 0));
    }

    #[test]
    fn the_pairs_card_keeps_the_six_closest_pairs_and_counts_the_rest() {
        // Eight pairs: six rows, three in each column, the closest tallies first.
        let pairs = vec![
            pair("u2", 9, 0),
            pair("u3", 4, 5),
            pair("u4", 7, 2),
            pair("u5", 3, 3),
            pair("u6", 0, 8),
            pair("u7", 2, 2),
            pair("u8", 6, 1),
            pair("u9", 0, 12),
        ];
        let data = feed(9, vec![], pairs, None);
        let m = build(&data, &me(), &roster, NOW).unwrap();
        assert_eq!(m.placement, Placement::Pairs);
        let order: Vec<&str> = m.pairs.iter().map(|p| p.opponent.id.as_str()).collect();
        assert_eq!(
            order,
            ["u5", "u7", "u3", "u4", "u8", "u6"],
            "gaps 0, 0, 1, 5, 5, 8"
        );
        assert_eq!(m.pairs_total, 8);
        let d = to_slint(&m, &|_| None);
        assert_eq!((d.pairs_total, d.more_pairs), (8, 2));
    }

    #[test]
    fn an_avatar_in_the_cache_reaches_every_octagon_of_the_entry() {
        let data = feed(2, vec![], vec![pair("u2", 1, 0)], None);
        let m = build(&data, &me(), &roster, NOW).unwrap();
        let img =
            slint::Image::from_rgba8(slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(2, 2));
        let only_u2 = |id: &str| (id == "u2").then(|| img.clone());
        let d = to_slint(&m, &only_u2);
        use slint::Model;
        let row = d.pairs.row_data(0).unwrap();
        assert!(row.has_avatar, "the opponent shows the image");
        assert!(!row.you_has_avatar, "the viewer has none, so initials");
        assert!(!d.left_has_avatar);
        let members = [Person::new("u2", "Jonas"), Person::new("me", "Bob")];
        let c = invite_card_data(&members, 2, &only_u2);
        assert!(c.members.row_data(0).unwrap().has_avatar);
        assert!(!c.members.row_data(1).unwrap().has_avatar);
    }

    fn moved_title(id: &str, holder: &str, previous: &str, days: i64, value: Value) -> Value {
        json!({
            "title_id": id, "holder_user_id": holder, "previous_holder_user_id": previous,
            "value": value, "unit": "", "since_ms": NOW - days * DAY_MS,
            "moved_at_ms": NOW - days * DAY_MS,
        })
    }

    #[test]
    fn the_newest_move_of_any_age_is_the_belt_hero_with_both_values() {
        // Frame E: Jonas took Night Owl from Bob two days ago.
        let moved = moved_title("night_owl", "u2", "me", 2, json!("03:40"));
        let data = json!({
            "crew_id": "c", "member_count": 4, "open_seats": 1,
            "titles": [moved, moved_title("the_carry", "u3", "u2", 9, json!(4201.0))],
            "pairs": [{"a": "me", "b": "u2", "a_count": 3, "b_count": 4, "values": [
                {"title_id": "night_owl", "a_value": "03:14", "b_value": "03:40"}]}],
        });
        let m = build(&data, &me(), &roster, NOW).expect("model");
        assert_eq!(m.placement, Placement::Pairs, "the pairs card stays wide");
        let b = m.belt.expect("a belt");
        assert_eq!(b.headline(), "Jonas took Night Owl from Bob");
        assert_eq!(b.detail(), "03:40 beats 03:14.");
        assert_eq!(b.moved_ms, NOW - 2 * DAY_MS);
        assert!(b.is_news(NOW));
        assert!(!b.is_news(NOW + 13 * DAY_MS), "two weeks on it is not news");
    }

    fn sealed_data(days: i64) -> Value {
        json!({
            "crew_id": "c", "member_count": 3, "open_seats": 2,
            "titles": [moved_title("night_owl", "u2", "u3", days, json!("03:40"))],
            "pairs": [{"a": "me", "b": "u2", "a_count": 0, "b_count": 1, "values": [
                {"title_id": "night_owl", "a_value": "01:00", "b_value": "03:40"}]}],
        })
    }

    fn chip_belts(d: &Head2headData) -> Vec<(String, bool)> {
        use slint::Model;
        d.pairs
            .iter()
            .filter(|p| p.has_chip)
            .map(|p| (p.chip.belt.key.to_string(), p.chip.belt.sealed))
            .collect()
    }

    #[test]
    fn a_chip_of_a_belt_that_moved_is_sealed_until_opened() {
        let data = sealed_data(1);
        let m = build(&data, &me(), &roster, NOW).unwrap();
        let key = crate::sealed::belt_key("night_owl", NOW - DAY_MS);
        let none = |_: &str| false;
        let seal = Seal {
            enabled: true,
            now_ms: NOW,
            opened: &none,
            crew_id: "",
            own_since_ms: 0,
        };
        let d = to_slint_sealed(&m, &|_| None, &seal);
        assert_eq!(chip_belts(&d), vec![(key.clone(), true)]);
        // Opened: the same key, not sealed.
        let opened = |k: &str| k == key;
        let seal = Seal {
            enabled: true,
            now_ms: NOW,
            opened: &opened,
            crew_id: "",
            own_since_ms: 0,
        };
        let d = to_slint_sealed(&m, &|_| None, &seal);
        assert_eq!(chip_belts(&d), vec![(key, false)]);
        // Nothing seals without a seal.
        assert!(!chip_belts(&to_slint(&m, &|_| None))[0].1);
    }

    #[test]
    fn a_move_of_seven_days_opens_silently() {
        let m = build(&sealed_data(7), &me(), &roster, NOW).unwrap();
        let none = |_: &str| false;
        let seal = Seal {
            enabled: true,
            now_ms: NOW,
            opened: &none,
            crew_id: "",
            own_since_ms: 0,
        };
        assert!(!chip_belts(&to_slint_sealed(&m, &|_| None, &seal))[0].1);
        let m = build(&sealed_data(6), &me(), &roster, NOW).unwrap();
        assert!(chip_belts(&to_slint_sealed(&m, &|_| None, &seal))[0].1);
    }

    #[test]
    fn the_belt_carries_the_value_to_count_up_and_both_people() {
        use slint::Model;
        let m = build(&sealed_data(1), &me(), &roster, NOW).unwrap();
        let d = to_slint(&m, &|_| None);
        let b = d.pairs.row_data(0).unwrap().chip.belt;
        assert_eq!((b.kind, b.amount as i32, b.decimals), (2, 220, 0));
        assert_eq!(b.holder.name.as_str(), roster("u2").name);
        assert_eq!(b.prev.name.as_str(), roster("u3").name);
        assert_eq!(b.stat.as_str(), "Latest local time of a last session.");
        assert_eq!(b.cells.row_count(), 90);
    }

    #[test]
    fn opening_a_belt_clears_the_dot_on_its_pair_rows() {
        let data = sealed_data(0);
        let key = crate::sealed::belt_key("night_owl", NOW);
        // The data above moved at NOW - 0 days: after the last open (seen 0).
        let seen = NOW - 1000;
        let unread = build_rows_sealed(&data, &me(), &roster, NOW + 5, seen, &|_| false).unwrap();
        assert!(unread.pairs[0].moved_recent);
        let read = build_rows_sealed(&data, &me(), &roster, NOW + 5, seen, &|k| k == key).unwrap();
        assert!(!read.pairs[0].moved_recent);
    }

    #[test]
    fn a_move_between_two_other_members_is_no_belt_hero() {
        let moved = moved_title("night_owl", "u2", "u3", 2, json!("03:40"));
        let data = feed(4, vec![moved], vec![pair("u2", 1, 1)], None);
        let m = build(&data, &me(), &roster, NOW).expect("model");
        assert!(m.belt.is_none());
    }

    #[test]
    fn a_belt_without_the_pair_values_has_no_detail_line() {
        let moved = moved_title("the_carry", "me", "u2", 3, json!(4201.0));
        let data = feed(4, vec![moved], vec![pair("u2", 1, 1)], None);
        let b = build(&data, &me(), &roster, NOW)
            .expect("model")
            .belt
            .expect("a belt");
        assert_eq!(b.headline(), "Bob took The Carry from Jonas");
        assert_eq!(b.detail(), "");
        assert_eq!(b.holder_value, "4,201");
    }
}
