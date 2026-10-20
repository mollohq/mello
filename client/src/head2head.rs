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
use crate::{H2hPairRow, H2hPerson, Head2headData, TitleHeld};

/// Seats head2head shows, as in the backend. A crew of five or more has none.
pub const SEATS: i32 = 5;
/// Most pair rows in the cell. With seats and the invite button the cell has
/// room for two rows, without them for three.
const ROWS_WITH_SEATS: usize = 2;
const ROWS_FULL: usize = 3;
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
    /// Two to four members: a standard cell with seats and the invite.
    Cell,
    /// Five or more members: a standard cell without seats and button, and
    /// the invite card beside it.
    CellFull,
}

impl Placement {
    pub fn variant(self) -> &'static str {
        match self {
            Placement::Wide => "wide",
            Placement::Versus => "versus",
            Placement::Cell => "cell",
            Placement::CellFull => "cell-full",
        }
    }
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
}

#[derive(Debug, Clone, PartialEq)]
pub struct PairRow {
    pub opponent: Person,
    pub you_count: i32,
    pub them_count: i32,
    /// The contested title that decides the pair, if any.
    pub deciding: Option<Chip>,
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
    }
}

/// Whether the higher value holds the title. Only has_a_life goes to the
/// fewest hours.
fn higher_wins(id: &str) -> bool {
    id != "has_a_life"
}

/// The contested title that decides a pair: the one with the largest relative
/// gap, `abs(a - b) / max(a, b)`, over the titles in the pair's values. Ties
/// go to the title that comes first in the display order. The chip carries
/// the value of the member who would hold the title in the pair, and the
/// held days of the crew holder when that is the same member.
fn deciding_chip(
    pair: &WirePair,
    titles: &[&WireTitle],
    viewer: &str,
    roster: &dyn Fn(&str) -> Person,
    now_ms: i64,
    moved_id: Option<&str>,
) -> Option<Chip> {
    let mut best: Option<(f64, &WirePairValue, &WireTitle)> = None;
    for v in &pair.values {
        let Some(crew) = titles.iter().find(|t| t.title_id == v.title_id) else {
            continue;
        };
        let (a, b) = (magnitude(&v.a_value), magnitude(&v.b_value));
        let top = a.max(b);
        let gap = if top > 0.0 { (a - b).abs() / top } else { 0.0 };
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
    }
    Some(chip)
}

/// Build the model from the entry data. `None` when the data does not parse.
pub fn build(
    data: &Value,
    viewer: &Person,
    roster: &dyn Fn(&str) -> Person,
    now_ms: i64,
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

    // The viewer is A in every pair. Rows keep the payload order.
    let mut pairs: Vec<PairRow> = wire
        .pairs
        .iter()
        .filter(|p| p.a == viewer.id)
        .map(|p| PairRow {
            opponent: roster(&p.b),
            you_count: p.a_count,
            them_count: p.b_count,
            deciding: deciding_chip(p, &known, &viewer.id, roster, now_ms, moved_id.as_deref()),
        })
        .collect();

    let belt_moved = wire.belt_moved_recent.is_some();
    let mut model = Model {
        placement: Placement::Cell,
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
        let other_id = if moved.holder_user_id != viewer.id {
            Some(moved.holder_user_id.clone())
        } else if !moved.previous_holder_user_id.is_empty() {
            Some(moved.previous_holder_user_id.clone())
        } else {
            pairs.first().map(|p| p.opponent.id.clone())
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
    pairs.sort_by_key(|p| (p.you_count - p.them_count).abs());
    if wire.member_count >= SEATS {
        model.placement = Placement::CellFull;
        model.open_seats = 0;
        pairs.truncate(ROWS_FULL);
    } else {
        pairs.truncate(ROWS_WITH_SEATS);
    }
    model.pairs = pairs;
    Some(model)
}

// ---- Slint ----

fn icon_of(id: &str) -> slint::Image {
    look(id)
        .and_then(|t| slint::Image::load_from_svg_data(t.icon_svg).ok())
        .unwrap_or_default()
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

fn title_held(c: &Chip) -> TitleHeld {
    TitleHeld {
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

/// The UI struct of the head2head card.
pub fn to_slint(m: &Model, avatar: &dyn Fn(&str) -> Option<slint::Image>) -> Head2headData {
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
            chip: p.deciding.as_ref().map(title_held).unwrap_or_default(),
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
        titles: model_of(m.titles.iter().map(title_held).collect()),
        chips: model_of(m.chips.iter().map(title_held).collect()),
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
        // Only the cell has a pair list to continue.
        more_pairs: match m.placement {
            Placement::Cell | Placement::CellFull => {
                m.pairs_total.saturating_sub(m.pairs.len()) as i32
            }
            _ => 0,
        },
        left_score: m.left_score,
        right_score: m.right_score,
        members: model_of(Vec::new()),
        more_members: 0,
    }
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
    fn two_members_without_a_move_are_a_cell_with_one_pair() {
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
        assert_eq!(m.placement, Placement::Cell);
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
        assert_eq!(m.placement, Placement::CellFull);
        assert_eq!(m.open_seats, 0);
        assert_eq!(m.pairs.len(), ROWS_FULL);
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
    fn a_pair_without_values_has_no_chip() {
        let data = feed(
            3,
            vec![title("the_carry", "u3", json!(9000.0), "h")],
            vec![pair("u2", 0, 0)],
            None,
        );
        let m = build(&data, &me(), &roster, NOW).unwrap();
        assert!(m.pairs[0].deciding.is_none());
        let data = json!({"member_count": 2, "open_seats": 3, "titles": [],
            "pairs": [{"a": "me", "b": "u2", "a_count": 0, "b_count": 0, "values": null}]});
        let m = build(&data, &me(), &roster, NOW).unwrap();
        assert!(m.pairs[0].deciding.is_none());
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
        assert!(m.pairs[0].deciding.is_none());
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
    fn the_cell_keeps_the_three_closest_pairs_and_counts_the_rest() {
        // Five members and more: three rows, the closest tallies first.
        let pairs = vec![
            pair("u2", 9, 0),
            pair("u3", 4, 5),
            pair("u4", 7, 2),
            pair("u5", 3, 3),
            pair("u6", 0, 8),
        ];
        let data = feed(6, vec![], pairs, None);
        let m = build(&data, &me(), &roster, NOW).unwrap();
        assert_eq!(m.placement, Placement::CellFull);
        let order: Vec<&str> = m.pairs.iter().map(|p| p.opponent.id.as_str()).collect();
        assert_eq!(order, ["u5", "u3", "u4"], "gaps 0, 1, 5");
        assert_eq!(m.pairs_total, 5);
        let d = to_slint(&m, &|_| None);
        assert_eq!((d.pairs_total, d.more_pairs), (5, 2));
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
}
