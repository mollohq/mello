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
    pub viewer: Person,
    /// Versus: the other member.
    pub other: Option<Person>,
    pub left_score: i32,
    pub right_score: i32,
}

/// A number with thousands separators: 4167 -> "4,167".
fn grouped(n: i64) -> String {
    let digits = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
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
fn value_text(value: &Value, unit: &str) -> (String, String) {
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
fn magnitude(t: &WireTitle) -> f64 {
    match &t.value {
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

/// The contested title that decides a pair: of the titles held by the two,
/// the one with the largest gap.
///
/// The payload carries the holder's value only, not the other member's. Until
/// it does, the gap is the held value measured against the largest held value
/// of the same unit among all titles, so "4,167 h" outranks "4.6 h". Ties go
/// to the title that comes first in the display order.
fn deciding_title<'a>(
    titles: &'a [WireTitle],
    viewer: &str,
    opponent: &str,
) -> Option<&'a WireTitle> {
    let scale = |unit: &str| {
        titles
            .iter()
            .filter(|t| t.unit == unit)
            .map(magnitude)
            .fold(0.0_f64, f64::max)
    };
    titles
        .iter()
        .filter(|t| t.holder_user_id == viewer || t.holder_user_id == opponent)
        .filter(|t| look(&t.title_id).is_some())
        .map(|t| {
            let s = scale(&t.unit);
            (if s > 0.0 { magnitude(t) / s } else { 0.0 }, t)
        })
        .fold(None, |best: Option<(f64, &WireTitle)>, (g, t)| match best {
            Some((bg, bt))
                if bg > g
                    || (bg == g && display_rank(&bt.title_id) <= display_rank(&t.title_id)) =>
            {
                Some((bg, bt))
            }
            _ => Some((g, t)),
        })
        .map(|(_, t)| t)
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
    let known_owned: Vec<WireTitle> = known.iter().map(|t| (*t).clone()).collect();
    let mut pairs: Vec<PairRow> = wire
        .pairs
        .iter()
        .filter(|p| p.a == viewer.id)
        .map(|p| PairRow {
            opponent: roster(&p.b),
            you_count: p.a_count,
            them_count: p.b_count,
            deciding: deciding_title(&known_owned, &viewer.id, &p.b)
                .map(|t| chip_of(t, roster, now_ms, moved_id.as_deref() == Some(&t.title_id))),
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
        viewer: viewer.clone(),
        other: None,
        left_score: 0,
        right_score: 0,
    };

    if wire.member_count < 2 {
        model.placement = Placement::Wide;
        model.open_seats = (SEATS - wire.member_count.max(1)).clamp(0, SEATS);
        model.chips = model
            .titles
            .iter()
            .filter(|c| c.holder_id == viewer.id)
            .take(CHIPS)
            .cloned()
            .collect();
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
        moved_recent: c.moved_recent,
    }
}

fn model_of<T: Clone + 'static>(items: Vec<T>) -> slint::ModelRc<T> {
    slint::ModelRc::new(slint::VecModel::from(items))
}

/// The UI struct of the head2head card.
pub fn to_slint(m: &Model) -> Head2headData {
    let pairs: Vec<H2hPairRow> = m
        .pairs
        .iter()
        .map(|p| H2hPairRow {
            name: p.opponent.name.as_str().into(),
            initials: p.opponent.initials.as_str().into(),
            hue_index: p.opponent.hue,
            you_initials: m.viewer.initials.as_str().into(),
            you_hue_index: m.viewer.hue,
            you_count: p.you_count,
            them_count: p.them_count,
            has_chip: p.deciding.is_some(),
            chip: p.deciding.as_ref().map(title_held).unwrap_or_default(),
        })
        .collect();
    let (right_name, right_initials, right_hue) = m
        .other
        .as_ref()
        .map(|o| (o.name.as_str(), o.initials.as_str(), o.hue))
        .unwrap_or(("", "", 0));
    Head2headData {
        variant: m.placement.variant().into(),
        member_count: m.member_count,
        open_seats: m.open_seats,
        belt_moved_recent: m.belt_moved_recent,
        titles: model_of(m.titles.iter().map(title_held).collect()),
        chips: model_of(m.chips.iter().map(title_held).collect()),
        pairs: model_of(pairs),
        left_name: m.viewer.name.as_str().into(),
        left_initials: m.viewer.initials.as_str().into(),
        left_hue_index: m.viewer.hue,
        right_name: right_name.into(),
        right_initials: right_initials.into(),
        right_hue_index: right_hue,
        left_score: m.left_score,
        right_score: m.right_score,
        members: model_of(Vec::new()),
        more_members: 0,
    }
}

/// The UI struct of the invite card: the first members as octagons.
pub fn invite_card_data(members: &[Person], member_count: i32) -> Head2headData {
    let shown: Vec<H2hPerson> = members
        .iter()
        .take(INVITE_CARD_MEMBERS)
        .map(|p| H2hPerson {
            name: p.name.as_str().into(),
            initials: p.initials.as_str().into(),
            hue_index: p.hue,
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

    #[test]
    fn the_deciding_chip_is_the_title_with_the_largest_gap() {
        let data = feed(
            2,
            vec![
                title("locked_in", "me", json!(4.6), "h"),
                title("the_carry", "u2", json!(4171.0), "h"),
                title("sampler", "u3", json!(9), "games"),
            ],
            vec![pair("u2", 1, 1)],
            None,
        );
        let m = build(&data, &me(), &roster, NOW).unwrap();
        let chip = m.pairs[0].deciding.as_ref().unwrap();
        assert_eq!(chip.id, "the_carry", "4,171 h against 4.6 h is the widest");
        assert_eq!(chip.holder_id, "u2");
        assert_eq!(chip.value, "4,171");
    }

    #[test]
    fn a_title_held_by_a_third_member_does_not_decide_the_pair() {
        let data = feed(
            3,
            vec![
                title("the_carry", "u3", json!(9000.0), "h"),
                title("the_collector", "u2", json!(3), "installed"),
            ],
            vec![pair("u2", 0, 1)],
            None,
        );
        let m = build(&data, &me(), &roster, NOW).unwrap();
        assert_eq!(m.pairs[0].deciding.as_ref().unwrap().id, "the_collector");
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
        let d = invite_card_data(&members, 11);
        use slint::Model;
        assert_eq!(d.members.row_count(), 8);
        assert_eq!(d.more_members, 3);
        let d = invite_card_data(&members[..5], 5);
        assert_eq!((d.members.row_count(), d.more_members), (5, 0));
    }
}
