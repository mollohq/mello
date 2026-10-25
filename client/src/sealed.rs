//! Sealed belts and the sealed weekly recap (plan `SEALED-REVEAL.md`).
//!
//! A belt that moved, and the weekly recap, show sealed in the feed until the
//! user opens them. This module owns what the UI needs to draw a sealed card:
//! the key of a move, its seed, the code cells as `Path` commands, and the data
//! of the centre view. The opened keys are local, by crew, in the settings.
//!
//! Nothing here reads a window, so the tests need none.

use std::collections::HashSet;

use serde_json::Value;

use crate::head2head::{self, Person};
use crate::settings::Settings;
use crate::{BeltReveal, H2hPerson, RecapSeal};

/// A move is sealed for this long. An older move opens silently.
pub const SEAL_DAYS: i64 = 7;
const DAY_MS: i64 = 86_400_000;
/// An opened key is kept until it is older than every card it can open. A
/// recap stays sealed for 7 days after its week ends, and its key holds the
/// start of that week: 14 days, and one day more.
const KEEP_DAYS: i64 = 2 * SEAL_DAYS + 1;

/// Columns and rows of the code of a belt. The chip shows the cells that fit.
pub const COLS: usize = 15;
pub const ROWS: usize = 6;
/// Columns and rows of the field of the sealed recap.
const RECAP_COLS: usize = 13;
const RECAP_ROWS: usize = 20;

/// The stable pseudo random number of the lab: the classic GLSL hash.
pub fn hash(n: f64) -> f64 {
    let v = (n * 12.9898).sin() * 43758.5453;
    v - v.floor()
}

/// FNV-1a over the bytes. Small, so the seed stays exact in a float.
fn fnv(parts: &[&[u8]]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for part in parts {
        for b in *part {
            h ^= u32::from(*b);
            h = h.wrapping_mul(0x0100_0193);
        }
    }
    h
}

/// The key of a belt move: one key for each move, in every slot.
pub fn belt_key(title_id: &str, moved_ms: i64) -> String {
    format!("{title_id}:{moved_ms}")
}

/// The key of the viewer's own value of a title on a crew. One key for a title
/// and a crew, with no value and no time in it: a chip that was opened once
/// stays open on later days, whatever the value is then.
pub fn own_key(title_id: &str, crew_id: &str) -> String {
    format!("own:{title_id}:{crew_id}")
}

/// The seed of an own chip. The same in every slot and every day.
pub fn own_seed(title_id: &str, crew_id: &str) -> i32 {
    (fnv(&[b"own", title_id.as_bytes(), crew_id.as_bytes()]) % 9973) as i32
}

/// The key of a weekly recap.
pub fn recap_key(week_start_ms: i64) -> String {
    format!("recap:{week_start_ms}")
}

/// The seed of a belt move.
pub fn belt_seed(title_id: &str, moved_ms: i64) -> i32 {
    (fnv(&[title_id.as_bytes(), &moved_ms.to_le_bytes()]) % 9973) as i32
}

/// The seed of a recap week.
pub fn recap_seed(week_start_ms: i64) -> i32 {
    (fnv(&[b"recap", &week_start_ms.to_le_bytes()]) % 9973) as i32
}

/// The cells of the code of a belt, row by row, `ROWS` by `COLS`. The stride
/// is fixed, so a chip that shows fewer columns shows the same cells.
pub fn belt_cells(seed: i32) -> Vec<bool> {
    (0..COLS * ROWS)
        .map(|i| hash(i as f64 * 1.37 + f64::from(seed) * 101.3 + 0.17) > 0.45)
        .collect()
}

/// `Path` commands of the cells: 4 units square on a pitch of 5. One path
/// draws the whole code.
pub fn cells_path(cells: &[bool]) -> String {
    let mut out = String::new();
    for (i, on) in cells.iter().enumerate() {
        if *on {
            let (c, r) = (i % COLS, i / COLS);
            out.push_str(&format!("M{} {}h4v4h-4z", c * 5, r * 5));
        }
    }
    out
}

/// The level of each cell of the sealed recap field, row by row: 0 none,
/// 1 dim, 2 bright. The key cell and the cells above the skip are 0.
pub fn recap_levels(seed: i32) -> Vec<i32> {
    const KEY_COL: usize = 10;
    const KEY_ROW: usize = 15;
    (0..RECAP_COLS * RECAP_ROWS)
        .map(|i| {
            let (c, r) = (i % RECAP_COLS, i / RECAP_COLS);
            let v = hash(i as f64 * 1.37 + f64::from(seed) * 57.1 + 0.31);
            // No cells above 70 of 248.
            if 8 + r * 12 < 70 || v <= 0.76 || (c == KEY_COL && r == KEY_ROW) {
                0
            } else if v > 0.93 {
                2
            } else {
                1
            }
        })
        .collect()
}

/// The field of the sealed recap as two paths, in a box of 160 by 248 units:
/// the bright cells and the dim cells.
pub fn recap_field(levels: &[i32]) -> (String, String) {
    let (mut bright, mut dim) = (String::new(), String::new());
    for (i, level) in levels.iter().enumerate() {
        let (x, y) = (14 + (i % RECAP_COLS) * 11, 8 + (i / RECAP_COLS) * 12);
        let cell = format!("M{x} {y}h5v5h-5z");
        match level {
            2 => bright.push_str(&cell),
            1 => dim.push_str(&cell),
            _ => {}
        }
    }
    (bright, dim)
}

/// The sentence under the value of a title: the Stat column of
/// `HEAD2HEAD.md` section 2.
pub fn stat_line(title_id: &str) -> &'static str {
    match title_id {
        "the_carry" => "Most hours in the top shared game.",
        "locked_in" => "Most game hours in the last 7 days.",
        "has_a_life" => "Fewest game hours in the last 7 days.",
        "night_owl" => "Latest local time of a last session.",
        "the_collector" => "Most installed games.",
        "sampler" => "Most different games in the last 14 days.",
        "one_trick" => "Highest share of play time in one game.",
        "mouth" => "Most voice minutes in the last 14 days.",
        "marathon" => "Longest single game session.",
        "last_one_up" => "Latest session end in the last 14 days.",
        "clipper" => "Most clips posted.",
        "rage_quit" => "Most sessions of 2 to 10 minutes in 14 days.",
        "winner" => "Most win moments in the last 14 days.",
        _ => "",
    }
}

/// How the value counts up in the centre view: the kind (0 count, 1 hours and
/// minutes, 2 clock), the number, and the decimals of a count.
pub fn count_of(value: &Value, unit: &str) -> (i32, f32, i32) {
    match value {
        Value::String(s) => {
            let mut it = s.split(':');
            let h: f32 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
            let m: f32 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
            (2, h * 60.0 + m, 0)
        }
        Value::Number(n) => {
            let f = n.as_f64().unwrap_or(0.0) as f32;
            match unit {
                "min" => (1, f, 0),
                "h" if f.abs() < 100.0 => (0, f, 1),
                _ => (0, f, 0),
            }
        }
        _ => (0, 0.0, 0),
    }
}

/// What decides if a move is sealed: the time, and the keys the user opened.
pub struct Seal<'a> {
    /// False seals nothing. Tests and previews that do not care use it.
    pub enabled: bool,
    pub now_ms: i64,
    pub opened: &'a dyn Fn(&str) -> bool,
    /// The crew of the feed, for the key of an own chip.
    pub crew_id: &'a str,
    /// When the viewer first saw the own chips of this crew sealed. 0: not yet.
    pub own_since_ms: i64,
}

impl Seal<'static> {
    /// A seal that seals nothing.
    #[cfg(test)]
    pub fn off() -> Self {
        Seal {
            enabled: false,
            now_ms: 0,
            opened: &|_| false,
            crew_id: "",
            own_since_ms: 0,
        }
    }
}

impl Seal<'_> {
    /// An own chip is sealed when it is not opened and the crew's day 0 reveal
    /// is under `SEAL_DAYS` old. After that it opens silently, like a move.
    pub fn is_own_sealed(&self, key: &str) -> bool {
        self.enabled
            && !self.crew_id.is_empty()
            && (self.own_since_ms == 0 || self.now_ms - self.own_since_ms < SEAL_DAYS * DAY_MS)
            && !(self.opened)(key)
    }

    /// A move is sealed when it is under `SEAL_DAYS` old and not opened.
    pub fn is_sealed(&self, key: &str, moved_ms: i64) -> bool {
        self.enabled
            && moved_ms > 0
            && self.now_ms - moved_ms < SEAL_DAYS * DAY_MS
            && !(self.opened)(key)
    }
}

/// The opened keys of a crew.
pub fn opened_keys(settings: &Settings, crew_id: &str) -> HashSet<String> {
    settings
        .sealed_opened
        .get(crew_id)
        .map(|v| v.iter().cloned().collect())
        .unwrap_or_default()
}

/// Store a key as opened. Keys older than `KEEP_DAYS` leave the list: a card
/// that old is not sealed whatever the list says. Returns true on a change.
pub fn mark_opened(settings: &mut Settings, crew_id: &str, key: &str, now_ms: i64) -> bool {
    let list = settings
        .sealed_opened
        .entry(crew_id.to_string())
        .or_default();
    let before = list.len();
    list.retain(|k| key_time_ms(k).is_none_or(|t| now_ms - t < KEEP_DAYS * DAY_MS));
    let mut changed = list.len() != before;
    if !list.iter().any(|k| k == key) {
        list.push(key.to_string());
        changed = true;
    }
    changed
}

/// Store the time the viewer first saw the own chips of a crew sealed, when no
/// time is stored yet. Returns the time that counts, and true on a change.
pub fn own_since(settings: &mut Settings, crew_id: &str, now_ms: i64) -> (i64, bool) {
    match settings.own_seal_since.get(crew_id) {
        Some(&t) => (t, false),
        None => {
            settings.own_seal_since.insert(crew_id.to_string(), now_ms);
            (now_ms, true)
        }
    }
}

/// The time inside a key: the move time, or the week start of a recap. An own
/// key has none: it never expires by age.
fn key_time_ms(key: &str) -> Option<i64> {
    if key.starts_with("own:") {
        return None;
    }
    key.rsplit(':').next()?.parse().ok()
}

/// One belt move, as the payload names it.
pub struct Move<'a> {
    pub title_id: &'a str,
    pub moved_ms: i64,
    pub holder: &'a Person,
    pub prev: &'a Person,
    pub value: &'a Value,
    pub unit: &'a str,
}

/// The moved belt of the UI. An opened belt has the same data, so the centre
/// view and a replay need nothing more.
pub fn reveal_of(
    m: &Move,
    avatar: &dyn Fn(&str) -> Option<slint::Image>,
    seal: &Seal,
) -> BeltReveal {
    let key = belt_key(m.title_id, m.moved_ms);
    let seed = belt_seed(m.title_id, m.moved_ms);
    let cells = belt_cells(seed);
    let (title, icon) = head2head::belt_look(m.title_id);
    let (kind, amount, decimals) = count_of(m.value, m.unit);
    // A clock has no unit. Minutes show as hours and minutes.
    let unit = if kind == 0 { m.unit } else { "" };
    let person = |p: &Person| H2hPerson {
        id: p.id.as_str().into(),
        name: p.name.as_str().into(),
        initials: p.initials.as_str().into(),
        hue_index: p.hue,
        avatar: avatar(&p.id).unwrap_or_default(),
        has_avatar: avatar(&p.id).is_some(),
    };
    BeltReveal {
        sealed: seal.is_sealed(&key, m.moved_ms),
        key: key.into(),
        seed,
        code: cells_path(&cells).into(),
        cells: slint::ModelRc::new(slint::VecModel::from(cells)),
        title: title.into(),
        icon,
        hue_index: m.holder.hue,
        kind,
        amount,
        decimals,
        unit: unit.into(),
        stat: stat_line(m.title_id).into(),
        holder: person(m.holder),
        prev: person(m.prev),
        own: false,
    }
}

/// The viewer's own value of a title, for the chip of a new crew.
pub struct OwnValue<'a> {
    pub title_id: &'a str,
    pub viewer: &'a Person,
    pub value: &'a Value,
    /// The unit as the server sends it, for the count-up.
    pub unit: &'a str,
    /// The unit as the chip shows it ("h this week").
    pub shown_unit: &'a str,
}

/// The own chip of the UI: the same back and the same reveal as a belt that
/// moved, in the viewer's own hue. The face has the viewer and "You", and no
/// previous holder.
pub fn own_reveal(
    o: &OwnValue,
    avatar: &dyn Fn(&str) -> Option<slint::Image>,
    seal: &Seal,
) -> BeltReveal {
    let key = own_key(o.title_id, seal.crew_id);
    let seed = own_seed(o.title_id, seal.crew_id);
    let cells = belt_cells(seed);
    let (title, icon) = head2head::belt_look(o.title_id);
    let (kind, amount, decimals) = count_of(o.value, o.unit);
    let unit = if kind == 0 { o.shown_unit } else { "" };
    BeltReveal {
        sealed: seal.is_own_sealed(&key),
        key: key.into(),
        seed,
        code: cells_path(&cells).into(),
        cells: slint::ModelRc::new(slint::VecModel::from(cells)),
        title: title.into(),
        icon,
        hue_index: o.viewer.hue,
        kind,
        amount,
        decimals,
        unit: unit.into(),
        stat: stat_line(o.title_id).into(),
        holder: H2hPerson {
            id: o.viewer.id.as_str().into(),
            name: "You".into(),
            initials: o.viewer.initials.as_str().into(),
            hue_index: o.viewer.hue,
            avatar: avatar(&o.viewer.id).unwrap_or_default(),
            has_avatar: avatar(&o.viewer.id).is_some(),
        },
        prev: Default::default(),
        own: true,
    }
}

/// The sealed state of a weekly recap, from its week start.
pub fn recap_seal(week_start_ms: i64, week_end_ms: i64, seal: &Seal) -> RecapSeal {
    let key = recap_key(week_start_ms);
    let seed = recap_seed(week_start_ms);
    let levels = recap_levels(seed);
    let (bright, dim) = recap_field(&levels);
    RecapSeal {
        levels: slint::ModelRc::new(slint::VecModel::from(levels)),
        sealed: week_start_ms > 0 && seal.is_sealed(&key, week_end_ms),
        key: key.into(),
        seed,
        bright: bright.into(),
        dim: dim.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1_760_000_000_000;

    #[test]
    fn a_move_has_one_key_and_one_seed() {
        assert_eq!(belt_key("night_owl", 42), "night_owl:42");
        assert_eq!(belt_seed("night_owl", 42), belt_seed("night_owl", 42));
        assert_ne!(belt_seed("night_owl", 42), belt_seed("night_owl", 43));
        assert!(belt_seed("the_carry", NOW) < 9973);
    }

    #[test]
    fn the_code_is_the_same_in_every_slot() {
        let (a, b) = (
            belt_cells(belt_seed("marathon", 7)),
            belt_cells(belt_seed("marathon", 7)),
        );
        assert_eq!(a, b);
        assert_eq!(a.len(), COLS * ROWS);
        let on = a.iter().filter(|c| **c).count();
        assert!(
            (30..=70).contains(&on),
            "about 55 percent of 90 cells, got {on}"
        );
        // One command for each lit cell.
        assert_eq!(cells_path(&a).matches('M').count(), on);
    }

    #[test]
    fn the_recap_field_has_no_cell_above_the_skip_and_none_on_the_key() {
        let (bright, dim) = recap_field(&recap_levels(recap_seed(NOW)));
        assert!(!dim.is_empty());
        for cell in bright
            .split('M')
            .chain(dim.split('M'))
            .filter(|c| !c.is_empty())
        {
            let mut it = cell.split([' ', 'h']).take(2);
            let x: i32 = it.next().unwrap().parse().unwrap();
            let y: i32 = it.next().unwrap().parse().unwrap();
            assert!(y >= 70, "cell at y {y}");
            assert!(
                !(x == 14 + 10 * 11 && y == 8 + 15 * 12),
                "the key cell is empty"
            );
        }
    }

    #[test]
    fn every_title_has_a_stat_sentence() {
        for id in [
            "the_carry",
            "locked_in",
            "has_a_life",
            "night_owl",
            "the_collector",
            "sampler",
            "one_trick",
            "mouth",
            "marathon",
            "last_one_up",
            "clipper",
            "rage_quit",
            "winner",
        ] {
            assert!(!stat_line(id).is_empty(), "{id}");
        }
        assert_eq!(stat_line("nope"), "");
    }

    #[test]
    fn the_count_up_kinds() {
        assert_eq!(count_of(&json!(4172), "h"), (0, 4172.0, 0));
        assert_eq!(count_of(&json!(4.6), "h"), (0, 4.6, 1));
        assert_eq!(count_of(&json!(245), "min"), (1, 245.0, 0));
        assert_eq!(count_of(&json!("03:22"), ""), (2, 202.0, 0));
        assert_eq!(count_of(&json!(12), "clips"), (0, 12.0, 0));
        assert_eq!(count_of(&Value::Null, ""), (0, 0.0, 0));
    }

    #[test]
    fn a_move_is_sealed_until_opened_or_seven_days_old() {
        let none = |_: &str| false;
        let seal = Seal {
            enabled: true,
            now_ms: NOW,
            opened: &none,
            crew_id: "",
            own_since_ms: 0,
        };
        assert!(seal.is_sealed("k", NOW - DAY_MS));
        assert!(
            !seal.is_sealed("k", NOW - 7 * DAY_MS),
            "seven days opens silently"
        );
        assert!(!seal.is_sealed("k", 0), "no move, no seal");
        let opened = |k: &str| k == "k";
        let seal = Seal {
            enabled: true,
            now_ms: NOW,
            opened: &opened,
            crew_id: "",
            own_since_ms: 0,
        };
        assert!(!seal.is_sealed("k", NOW - DAY_MS));
        assert!(seal.is_sealed("other", NOW - DAY_MS));
        assert!(!Seal::off().is_sealed("k", NOW - DAY_MS));
    }

    #[test]
    fn opened_keys_are_stored_by_crew_and_old_ones_leave() {
        let mut s = Settings::default();
        let old = belt_key("night_owl", NOW - 20 * DAY_MS);
        let fresh = belt_key("the_carry", NOW - DAY_MS);
        let recap = recap_key(NOW - 13 * DAY_MS);
        assert!(mark_opened(&mut s, "c1", &old, NOW - 20 * DAY_MS));
        assert!(mark_opened(&mut s, "c1", &recap, NOW - 12 * DAY_MS));
        assert!(mark_opened(&mut s, "c1", &fresh, NOW));
        assert!(
            !mark_opened(&mut s, "c1", &fresh, NOW),
            "the same key twice is no change"
        );
        let keys = opened_keys(&s, "c1");
        assert!(keys.contains(&fresh));
        assert!(!keys.contains(&old), "a key older than 15 days is dropped");
        assert!(
            keys.contains(&recap),
            "the key of a recap outlives its week"
        );
        assert!(opened_keys(&s, "c2").is_empty());
    }

    #[test]
    fn the_opened_keys_survive_a_restart() {
        let mut s = Settings::default();
        let key = belt_key("night_owl", NOW);
        mark_opened(&mut s, "c1", &key, NOW);
        // The settings file is JSON in the config dir.
        let stored = serde_json::to_string(&s).expect("settings serialize");
        let back: Settings = serde_json::from_str(&stored).expect("settings parse");
        assert!(opened_keys(&back, "c1").contains(&key));
        // A settings file from before this feature has no keys.
        let old: Settings = serde_json::from_str("{}").expect("old settings parse");
        assert!(opened_keys(&old, "c1").is_empty());
    }

    #[test]
    fn an_own_key_has_no_value_and_no_time_so_it_never_expires() {
        assert_eq!(own_key("the_carry", "c1"), "own:the_carry:c1");
        assert_eq!(own_seed("the_carry", "c1"), own_seed("the_carry", "c1"));
        assert_ne!(own_seed("the_carry", "c1"), own_seed("the_carry", "c2"));
        let mut s = Settings::default();
        // A crew id that is all digits must not read as a time.
        let key = own_key("night_owl", "12345");
        assert!(mark_opened(&mut s, "12345", &key, NOW));
        assert!(mark_opened(
            &mut s,
            "12345",
            &belt_key("x", NOW),
            NOW + 40 * DAY_MS
        ));
        assert!(opened_keys(&s, "12345").contains(&key));
    }

    #[test]
    fn an_own_chip_is_sealed_until_opened_or_seven_days_after_the_first_look() {
        static NONE: fn(&str) -> bool = |_| false;
        let none = &NONE;
        let seal = |since: i64, opened: &'static dyn Fn(&str) -> bool| Seal {
            enabled: true,
            now_ms: NOW,
            opened,
            crew_id: "c1",
            own_since_ms: since,
        };
        let key = own_key("the_carry", "c1");
        assert!(seal(0, none).is_own_sealed(&key), "not stored yet");
        assert!(seal(NOW - DAY_MS, none).is_own_sealed(&key));
        assert!(!seal(NOW - 7 * DAY_MS, none).is_own_sealed(&key));
        assert!(!seal(NOW - DAY_MS, &|k: &str| k == "own:the_carry:c1").is_own_sealed(&key));
        assert!(!Seal::off().is_own_sealed(&key));
    }

    #[test]
    fn the_first_look_is_stored_once() {
        let mut s = Settings::default();
        assert_eq!(own_since(&mut s, "c1", NOW), (NOW, true));
        assert_eq!(own_since(&mut s, "c1", NOW + DAY_MS), (NOW, false));
        assert_eq!(own_since(&mut s, "c2", NOW + DAY_MS), (NOW + DAY_MS, true));
    }
}
