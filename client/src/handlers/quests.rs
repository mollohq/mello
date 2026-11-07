//! The quest card of a new crew (plans/crew-quests.md).
//!
//! The server sends `zones.quests` while the card's 14 days run. This module
//! turns it into the card: five gifts on a track, five step rows, the count
//! and the rail. The opened state of a gift is on the device (`sealed.rs`,
//! key `quest:<step>:<crew>`), so the client decides when the card goes:
//! when every step is done and, for the owner, every gift is opened.
//!
//! `build` is pure: the tests drive it with no window.

use mello_core::crew_events::{QuestCall, QuestGifts, ZoneQuests};
use slint::Image;

use crate::sealed;
use crate::{H2hPerson, QuestGift, QuestReveal, QuestRow};

const DAY_MS: i64 = 86_400_000;

/// An image of the media bucket (a banner, a crew emoji), from the cache.
/// The first ask sends `FetchMediaImage`; the answer builds the zones again.
pub(crate) fn media_image(ctx: &crate::app_context::AppContext, url: &str) -> Option<Image> {
    if url.is_empty() {
        return None;
    }
    if let Some(entry) = ctx.media_images.borrow().get(url) {
        return entry.clone();
    }
    ctx.media_images.borrow_mut().insert(url.to_string(), None);
    let _ = ctx.cmd_tx.send(mello_core::Command::FetchMediaImage {
        url: url.to_string(),
    });
    None
}

/// The banner image of a crew: a game from the art cache, or an uploaded
/// image from the media loader. Either may arrive later.
pub(crate) fn fill_banner(ctx: &crate::app_context::AppContext, c: &mut crate::CrewData) {
    let img = if !c.banner_game.is_empty() {
        crate::game_icons::resolve_art_worker(
            ctx,
            &[c.banner_game.to_string()],
            mello_core::art::ArtSlot::Hero,
        )
    } else {
        media_image(ctx, c.banner_url.as_str())
    };
    c.has_banner = img.is_some();
    c.banner = img.unwrap_or_default();
}

/// Fill the banner of every crew again: an image arrived.
pub(crate) fn refresh_banners(ctx: &crate::app_context::AppContext) {
    use slint::Model as _;
    let crews = ctx.app.get_crews();
    for i in 0..crews.row_count() {
        let Some(mut c) = crews.row_data(i) else {
            continue;
        };
        if c.banner_game.is_empty() && c.banner_url.is_empty() && !c.has_banner {
            continue;
        }
        let had = c.has_banner;
        fill_banner(ctx, &mut c);
        if had != c.has_banner || c.has_banner {
            crews.set_row_data(i, c);
        }
    }
}

/// The crew emoji of the active crew changed (crew state): keep them, ask
/// for their images, and build the picker and the chat again.
pub(crate) fn set_crew_emoji(
    ctx: &crate::app_context::AppContext,
    emoji: Vec<mello_core::crew_state::CrewEmoji>,
    owner: bool,
) {
    *ctx.crew_emoji.borrow_mut() = emoji;
    ctx.crew_emoji_owner.set(owner);
    refresh_crew_emoji(ctx);
}

/// Build the crew emoji of the picker and the add screen, and the chat rows,
/// from the list and the loaded images.
pub(crate) fn refresh_crew_emoji(ctx: &crate::app_context::AppContext) {
    use slint::ComponentHandle as _;
    let viewer = ctx.app.get_user_id().to_string();
    let owner = ctx.crew_emoji_owner.get();
    let rows: Vec<crate::CrewEmojiData> = ctx
        .crew_emoji
        .borrow()
        .iter()
        .map(|e| {
            let img = media_image(ctx, &e.url);
            crate::CrewEmojiData {
                name: e.name.clone().into(),
                has_image: img.is_some(),
                image: img.unwrap_or_default(),
                added_by: if e.added_by == viewer {
                    "You".into()
                } else {
                    e.added_by_name.clone().into()
                },
                can_remove: owner || e.added_by == viewer,
            }
        })
        .collect();
    let ui = ctx.app.global::<crate::CrewEmojiUi>();
    ui.set_crew_name(ctx.app.get_active_crew_name());
    ui.set_free((EMOJI_SLOTS - rows.len() as i32).max(0));
    ui.set_emoji(slint::ModelRc::new(slint::VecModel::from(rows)));
    ui.set_enabled(emoji_gift_open(ctx));
    super::chat::refresh_chat_ui(ctx);
}

/// Turn the add-emoji slots on or off from the newest feed. A feed load can
/// open the gift (the first call is done) with no crew state load after it.
pub(crate) fn refresh_emoji_gift(ctx: &crate::app_context::AppContext) {
    use slint::ComponentHandle as _;
    ctx.app
        .global::<crate::CrewEmojiUi>()
        .set_enabled(emoji_gift_open(ctx));
}

/// Five crew emoji for every crew (crew_gifts.go).
const EMOJI_SLOTS: i32 = 5;

/// The emoji gift is open: the crew has no quests (made before they
/// shipped, or the feed says nothing), or its first call is done.
fn emoji_gift_open(ctx: &crate::app_context::AppContext) -> bool {
    let active = ctx.app.get_active_crew_id();
    ctx.last_feed
        .borrow()
        .as_ref()
        .filter(|f| f.crew_id == active.as_str())
        .and_then(|f| f.zones.quests.as_ref())
        .is_none_or(|q| q.steps.call.is_some())
}

/// The events of the crew quests gifts.
pub(crate) fn handle(ctx: &crate::app_context::AppContext, event: mello_core::events::Event) {
    use mello_core::events::Event;
    match event {
        Event::MediaImageLoaded { url, bytes } => {
            let img = (!bytes.is_empty())
                .then(|| crate::image_cache::decode_media(&bytes))
                .flatten()
                .and_then(|d| d.frames.into_iter().next())
                .map(|(rgba, w, h)| crate::avatar::rgba_to_image(&rgba, w, h));
            if img.is_none() {
                log::warn!("[gifts] media image not usable: {url}");
            }
            ctx.media_images.borrow_mut().insert(url, img);
            super::zones::refresh(ctx);
            refresh_banners(ctx);
            refresh_crew_emoji(ctx);
        }
        Event::CrewGiftChanged { crew_id, gift } => {
            log::info!("[gifts] {gift} changed for crew {crew_id}");
            crate::callbacks::on_gift_result(ctx, &gift, None);
        }
        Event::CrewGiftFailed {
            crew_id,
            gift,
            reason,
        } => {
            log::warn!("[gifts] {gift} failed for crew {crew_id}: {reason}");
            crate::callbacks::on_gift_result(ctx, &gift, Some(&reason));
        }
        Event::InviteNameChecked {
            crew_id,
            name,
            state,
        } => {
            log::debug!("[gifts] invite name {name} for crew {crew_id}: {state}");
            crate::callbacks::on_invite_name_checked(ctx, &name, &state);
        }
        _ => {}
    }
}

/// The steps in order. Each step unlocks the gift at the same index.
pub(crate) const STEPS: [&str; 5] = ["made", "friend", "call", "clip", "session"];
/// The name of a gift, shown once it is open. Until then it is "DROP 1" to
/// "DROP 5": what is in it stays a surprise.
const NAMES: [&str; 5] = [
    "BANNER",
    "INVITE LINK",
    "CREW EMOJI",
    "FIRST WEEK",
    "M3LLO+ 30 D",
];
const CONTENT: [&str; 5] = ["art", "link", "emoji", "week", "plus"];

/// The quest card as the zones draw it.
#[derive(Debug, Default, Clone)]
pub(crate) struct Card {
    pub count: String,
    pub fill: f32,
    pub gifts: Vec<QuestGift>,
    pub rows: Vec<QuestRow>,
}

/// What `build` reads besides the payload.
pub(crate) struct Input<'a> {
    pub crew_id: &'a str,
    pub crew_name: &'a str,
    pub viewer_id: &'a str,
    pub opened: &'a dyn Fn(&str) -> bool,
    /// The game clip key as Keybinds shows it ("F8"), or "".
    pub clip_key: &'a str,
    /// The viewer shares game activity (spec 17). Step 5 needs it.
    pub activity_on: bool,
    /// The octagon of a member by id, when the roster has them.
    pub person: &'a dyn Fn(&str) -> Option<H2hPerson>,
    /// The banner art, when a banner of game art is in the art cache.
    pub banner_art: Option<Image>,
    /// The first crew emoji, when it is loaded.
    pub emoji_art: Option<Image>,
    /// The header art of a game by id, when the art cache has it.
    pub game_art: &'a dyn Fn(&str) -> Option<Image>,
}

/// The key of the gift of a step on a crew.
pub(crate) fn gift_key(step: &str, crew_id: &str) -> String {
    sealed::quest_key(step, crew_id)
}

/// Which steps are done, in `STEPS` order. Crew made is always done.
fn done(q: &ZoneQuests) -> [bool; 5] {
    [
        true,
        q.steps.friend.is_some(),
        q.steps.call.is_some(),
        q.steps.clip.is_some(),
        q.steps.session.is_some(),
    ]
}

/// A member sees a done gift open once it holds something on the server:
/// the owner opened it and used it. The week and m3llo+ hold their gift
/// when the step is done.
fn has_content(i: usize, g: &QuestGifts) -> bool {
    match i {
        0 => !g.banner_url.is_empty(),
        1 => !g.invite_name.is_empty(),
        2 => g.emoji_count > 0,
        _ => true,
    }
}

/// The card, or `None` when it goes: every step done and, for the owner,
/// every gift opened.
pub(crate) fn build(q: &ZoneQuests, inp: &Input) -> Option<Card> {
    let done = done(q);
    let owner = !q.owner_id.is_empty() && q.owner_id == inp.viewer_id;
    let opened: Vec<bool> = STEPS
        .iter()
        .map(|s| (inp.opened)(&gift_key(s, inp.crew_id)))
        .collect();
    if done.iter().all(|d| *d) && (!owner || opened.iter().all(|o| *o)) {
        return None;
    }

    let count_done = done.iter().filter(|d| **d).count();
    let gifts = (0..5)
        .map(|i| {
            let mut g = gift(i, q, inp, done[i], owner, opened[i]);
            if g.state == "ready" && g.can_open {
                g.reveal = reveal(i, q, inp, &g, count_done);
            }
            g
        })
        .collect();
    let rows = vec![
        made_row(inp),
        friend_row(q, inp),
        call_row(q, inp),
        clip_row(q, inp),
        session_row(q, inp),
    ];
    let last = done.iter().rposition(|d| *d).unwrap_or(0);
    Some(Card {
        count: format!("{count_done} / 5"),
        fill: last as f32 / 4.0,
        gifts,
        rows,
    })
}

fn gift(i: usize, q: &ZoneQuests, inp: &Input, done: bool, owner: bool, opened: bool) -> QuestGift {
    let key = gift_key(STEPS[i], inp.crew_id);
    let seed = sealed::quest_seed(STEPS[i], inp.crew_id);
    let mut g = QuestGift {
        key: key.into(),
        label: format!("DROP {}", i + 1).into(),
        big: i == 4,
        content: CONTENT[i].into(),
        ..Default::default()
    };
    let open = done
        && if owner {
            opened
        } else {
            has_content(i, &q.gifts)
        };
    if !done {
        let (_, dim) = sealed::gift_field(seed, false, false);
        g.state = "locked".into();
        g.dim = dim.into();
    } else if !open {
        let (bright, dim) = sealed::gift_field(seed, true, owner);
        g.state = "ready".into();
        g.can_open = owner;
        g.bright = bright.into();
        g.dim = dim.into();
    } else {
        g.state = "open".into();
        g.label = NAMES[i].into();
        match i {
            0 => {
                if let Some(art) = &inp.banner_art {
                    g.art = art.clone();
                    g.has_art = true;
                }
            }
            2 => {
                if let Some(art) = &inp.emoji_art {
                    g.art = art.clone();
                    g.has_art = true;
                }
            }
            3 => {
                let week = q.gifts.founding_weeks.first().copied().unwrap_or(q.made_ms);
                g.text = format!("W{}", iso_week(week)).into();
            }
            4 => g.text = "30 DAYS".into(),
            _ => {}
        }
    }
    g
}

/// The face of a ready gift in the centre view: the moment of its step and
/// the gift (mockup feed-quests.html, frames 3 to 7).
fn reveal(i: usize, q: &ZoneQuests, inp: &Input, g: &QuestGift, done: usize) -> QuestReveal {
    let hue_of = |id: &str| (inp.person)(id).map(|p| p.hue_index).unwrap_or(0);
    let other = |ids: &[String]| ids.iter().find(|id| *id != inp.viewer_id).cloned();
    let mut r = QuestReveal {
        step: STEPS[i].into(),
        label: format!("QUEST  ·  {}", STEP_NAMES[i].to_uppercase()).into(),
        hue_index: hue_of(inp.viewer_id),
        count: format!("{done} / 5").into(),
        content: CONTENT[i].into(),
        bright: g.bright.clone(),
        dim: g.dim.clone(),
        ..Default::default()
    };
    let set_person = |r: &mut QuestReveal, id: &str| {
        if let Some(p) = (inp.person)(id) {
            r.moment_person = p;
            r.moment_has_person = true;
        }
    };
    match i {
        0 => {
            r.moment_title = inp.crew_name.into();
            r.moment_sub = format!("Made {}", day_month(q.made_ms)).into();
            r.name = "Crew banner".into();
            if let Some(art) = &inp.banner_art {
                r.gift_art = art.clone();
                r.gift_has_art = true;
            }
            r.action1 = "banner-game".into();
            r.action1_label = "Game art".into();
            r.action2 = "banner-upload".into();
            r.action2_label = "Upload".into();
        }
        1 => {
            if let Some(f) = &q.steps.friend {
                r.hue_index = hue_of(&f.user_id);
                set_person(&mut r, &f.user_id);
                r.moment_title = format!("{} joined", who(&f.user_id, &f.name, inp)).into();
                if !f.invited_by.is_empty() {
                    r.moment_sub = format!(
                        "Invited by {}",
                        who(&f.invited_by, &f.invited_by_name, inp).replace("You", "you")
                    )
                    .into();
                }
            }
            r.name = "Your invite link".into();
            r.gift_text = if q.gifts.invite_name.is_empty() {
                suggest_invite_name(inp.crew_name)
            } else {
                q.gifts.invite_name.clone()
            }
            .into();
            r.action1 = "link".into();
            r.action1_label = "Set the link".into();
        }
        2 => {
            if let Some(c) = &q.steps.call {
                if let Some(o) = other(&c.user_ids) {
                    r.hue_index = hue_of(&o);
                }
                r.moment_title = call_people(c, inp).into();
                r.moment_sub = c.channel_name.clone().into();
            }
            r.name = "Crew emoji".into();
            r.gift_line = "5 slots".into();
            r.action1 = "emoji".into();
            r.action1_label = "Add emoji".into();
        }
        3 => {
            if let Some(c) = &q.steps.clip {
                r.hue_index = hue_of(&c.clipper_id);
                r.moment_title =
                    format!("{} clipped", who(&c.clipper_id, &c.clipper_name, inp)).into();
                r.moment_sub = c.game.clone().into();
            }
            r.name = "Founding week".into();
            r.gift_text = g.text.clone();
            if r.gift_text.is_empty() {
                let week = q.gifts.founding_weeks.first().copied().unwrap_or(q.made_ms);
                r.gift_text = format!("W{}", iso_week(week)).into();
            }
            r.gift_line = "Open in Crew history for every member".into();
        }
        _ => {
            if let Some(s) = &q.steps.session {
                if let Some(o) = other(&s.user_ids) {
                    r.hue_index = hue_of(&o);
                }
                if let Some(art) = (inp.game_art)(&s.game_id) {
                    r.moment_art = art;
                    r.moment_has_art = true;
                }
                let span = if s.minutes >= 60 {
                    format!("{} h {:02}", s.minutes / 60, s.minutes % 60)
                } else {
                    format!("{} min", s.minutes)
                };
                r.moment_title = if s.game_name.is_empty() {
                    span
                } else {
                    format!("{span} · {}", s.game_name)
                }
                .into();
                r.moment_sub = names_line(&s.user_ids, &s.names, inp).into();
            }
            r.name = "For the crew".into();
            r.gift_text = "30 DAYS".into();
            r.gift_line = "30 days of m3llo+\nfor every member in the crew".into();
        }
    }
    r
}

/// The step names of the rows, in `STEPS` order.
const STEP_NAMES: [&str; 5] = [
    "Crew made",
    "A friend joins",
    "First call",
    "First clip",
    "First session",
];

/// A name for the invite link made from the crew name: lower case letters
/// and digits, 3 to 24 of them (the server's rule, crew_gifts.go).
fn suggest_invite_name(crew_name: &str) -> String {
    let mut out: String = crew_name
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .take(24)
        .collect();
    while out.len() < 3 {
        out.push('x');
    }
    out
}

/// "5 November" of a time in ms (UTC).
fn day_month(ms: i64) -> String {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let (_, m, d) = civil_from_days(ms.div_euclid(DAY_MS));
    format!("{d} {}", MONTHS[(m - 1) as usize])
}

/// The date of a day count (H. Hinnant, civil_from_days): year, month, day.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// The ISO week number of a time in ms (UTC): the week of its Thursday,
/// counted from the week of 4 January.
fn iso_week(ms: i64) -> i64 {
    let days = ms.div_euclid(DAY_MS);
    // 1970-01-01 was a Thursday: day 0 is weekday 3 when Monday is 0.
    let thursday = days - (days + 3).rem_euclid(7) + 3;
    let jan1 = days_from_civil(civil_year(thursday), 1, 1);
    (thursday - jan1) / 7 + 1
}

/// The year of a day count.
fn civil_year(days: i64) -> i64 {
    civil_from_days(days).0
}

/// The day count of a date (H. Hinnant, days_from_civil).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn row(name: &str) -> QuestRow {
    QuestRow {
        name: name.into(),
        ..Default::default()
    }
}

/// "You" for the viewer, else the name.
fn who(id: &str, name: &str, inp: &Input) -> String {
    if id == inp.viewer_id {
        "You".into()
    } else {
        name.into()
    }
}

/// "You and Ida", "You, Ida and Max", with the viewer first.
fn names_line(ids: &[String], names: &[String], inp: &Input) -> String {
    let mut out: Vec<String> = Vec::new();
    if ids.iter().any(|id| id == inp.viewer_id) {
        out.push("You".into());
    }
    for (id, name) in ids.iter().zip(names) {
        if id != inp.viewer_id {
            out.push(name.clone());
        }
    }
    match out.as_slice() {
        [] => String::new(),
        [a] => a.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

fn made_row(inp: &Input) -> QuestRow {
    QuestRow {
        done: true,
        meta: inp.crew_name.into(),
        ..row("Crew made")
    }
}

fn friend_row(q: &ZoneQuests, inp: &Input) -> QuestRow {
    let Some(f) = &q.steps.friend else {
        return QuestRow {
            meta: "Works in the browser".into(),
            action: "invite".into(),
            ..row("A friend joins")
        };
    };
    let mut r = QuestRow {
        done: true,
        title: format!("{} joined", who(&f.user_id, &f.name, inp)).into(),
        ..row("A friend joins")
    };
    if let Some(p) = (inp.person)(&f.user_id) {
        r.person = p;
        r.has_person = true;
    }
    r
}

fn call_row(q: &ZoneQuests, inp: &Input) -> QuestRow {
    let Some(c) = &q.steps.call else {
        return QuestRow {
            action: "voice".into(),
            ..row("First call")
        };
    };
    let mut meta = call_people(c, inp);
    if !c.channel_name.is_empty() {
        meta = format!("{meta} · {}", c.channel_name);
    }
    QuestRow {
        done: true,
        meta: meta.into(),
        ..row("First call")
    }
}

/// Who was in the first call: "You and Ida, +1 guest".
fn call_people(c: &QuestCall, inp: &Input) -> String {
    let mut out = names_line(&c.user_ids, &c.names, inp);
    if c.guests > 0 {
        let g = if c.guests == 1 { "guest" } else { "guests" };
        out = format!("{out}, +{} {g}", c.guests);
    }
    out
}

fn clip_row(q: &ZoneQuests, inp: &Input) -> QuestRow {
    let Some(c) = &q.steps.clip else {
        return QuestRow {
            meta: "Join voice to clip".into(),
            key_cap: inp.clip_key.into(),
            ..row("First clip")
        };
    };
    let by = who(&c.clipper_id, &c.clipper_name, inp);
    let meta = if c.game.is_empty() {
        by
    } else {
        format!("{by} · {}", c.game)
    };
    QuestRow {
        done: true,
        meta: meta.into(),
        ..row("First clip")
    }
}

fn session_row(q: &ZoneQuests, inp: &Input) -> QuestRow {
    let Some(s) = &q.steps.session else {
        return if inp.activity_on {
            QuestRow {
                meta: "Play with m3llo open".into(),
                ..row("First session")
            }
        } else {
            QuestRow {
                meta: "Game activity is off".into(),
                action: "settings".into(),
                ..row("First session")
            }
        };
    };
    let value = if s.minutes >= 60 {
        format!("{} h {:02}", s.minutes / 60, s.minutes % 60)
    } else {
        format!("{} min", s.minutes)
    };
    let mut meta = names_line(&s.user_ids, &s.names, inp);
    if !s.game_name.is_empty() {
        meta = format!("{meta} · {}", s.game_name);
    }
    QuestRow {
        done: true,
        meta: meta.into(),
        value: value.into(),
        ..row("First session")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mello_core::crew_events::{QuestFriend, QuestSession};
    use std::collections::HashSet;

    fn quests(owner: &str) -> ZoneQuests {
        ZoneQuests {
            owner_id: owner.into(),
            made_ms: 1_793_880_000_000,
            ends_ms: 1_793_880_000_000 + 14 * DAY_MS,
            ..Default::default()
        }
    }

    fn input<'a>(viewer: &'a str, opened: &'a dyn Fn(&str) -> bool) -> Input<'a> {
        Input {
            crew_id: "c1",
            crew_name: "bob_test_26",
            viewer_id: viewer,
            opened,
            clip_key: "F8",
            activity_on: true,
            person: &|_| None,
            banner_art: None,
            emoji_art: None,
            game_art: &|_| None,
        }
    }

    fn states(c: &Card) -> Vec<String> {
        c.gifts.iter().map(|g| g.state.to_string()).collect()
    }

    #[test]
    fn a_new_crew_is_one_of_five_with_the_banner_ready() {
        let none = |_: &str| false;
        let c = build(&quests("own"), &input("own", &none)).expect("the card shows");
        assert_eq!(c.count, "1 / 5");
        assert_eq!(c.fill, 0.0);
        assert_eq!(
            states(&c),
            ["ready", "locked", "locked", "locked", "locked"]
        );
        assert!(c.gifts[0].can_open, "the owner opens the banner");
        assert!(!c.gifts[0].bright.is_empty(), "a ready gift has its field");
        assert!(c.gifts[4].big, "the last gift is the big one");
        let labels: Vec<String> = c.gifts.iter().map(|g| g.label.to_string()).collect();
        assert_eq!(
            labels,
            ["DROP 1", "DROP 2", "DROP 3", "DROP 4", "DROP 5"],
            "a gift is a mystery until it is open"
        );
        assert_eq!(c.rows[1].action, "invite");
        assert_eq!(c.rows[2].action, "voice");
        assert_eq!(c.rows[3].key_cap, "F8");
    }

    #[test]
    fn an_opened_gift_is_open_for_the_owner() {
        let opened = |k: &str| k == "quest:made:c1";
        let mut q = quests("own");
        q.steps.friend = Some(QuestFriend {
            user_id: "ida".into(),
            name: "Ida".into(),
            ..Default::default()
        });
        let c = build(&q, &input("own", &opened)).expect("card");
        assert_eq!(states(&c), ["open", "ready", "locked", "locked", "locked"]);
        assert_eq!(c.gifts[0].label, "BANNER", "an open gift shows its name");
        assert_eq!(
            c.gifts[1].label, "DROP 2",
            "a ready gift is still a mystery"
        );
        assert_eq!(c.count, "2 / 5");
        assert_eq!(c.fill, 0.25);
        assert_eq!(c.rows[1].title, "Ida joined");
        assert!(c.rows[1].done);
    }

    #[test]
    fn a_member_never_gets_the_key() {
        // Ida's view: the owner opened the link and set a name, the emoji gift
        // has nothing in it yet.
        let none = |_: &str| false;
        let mut q = quests("own");
        q.steps.friend = Some(QuestFriend {
            user_id: "ida".into(),
            name: "Ida".into(),
            ..Default::default()
        });
        q.steps.call = Some(QuestCall {
            user_ids: vec!["own".into(), "ida".into()],
            names: vec!["b0bben".into(), "Ida".into()],
            channel_name: "General".into(),
            ..Default::default()
        });
        q.gifts.banner_url = "https://art.example/steam-1/hero".into();
        q.gifts.invite_name = "brawlbros".into();
        let c = build(&q, &input("ida", &none)).expect("card");
        assert_eq!(states(&c), ["open", "open", "ready", "locked", "locked"]);
        assert!(c.gifts.iter().all(|g| !g.can_open), "only the owner opens");
        assert_eq!(c.rows[1].title, "You joined");
        assert_eq!(c.rows[2].meta, "You and b0bben · General");
        assert_eq!(c.rows[2].value, "", "a call is marked on join: no length");
    }

    #[test]
    fn the_card_goes_when_all_is_done_and_opened() {
        let mut q = quests("own");
        q.steps.friend = Some(Default::default());
        q.steps.call = Some(Default::default());
        q.steps.clip = Some(Default::default());
        q.steps.session = Some(QuestSession {
            minutes: 125,
            ..Default::default()
        });
        let some: HashSet<&str> = [
            "quest:made:c1",
            "quest:friend:c1",
            "quest:call:c1",
            "quest:clip:c1",
        ]
        .into();
        let four = |k: &str| some.contains(k);
        let c = build(&q, &input("own", &four)).expect("the owner has a gift to open");
        assert_eq!(c.count, "5 / 5");
        assert_eq!(c.fill, 1.0);
        let all = |_: &str| true;
        assert!(
            build(&q, &input("own", &all)).is_none(),
            "all opened: the card goes"
        );
        let none = |_: &str| false;
        assert!(
            build(&q, &input("ida", &none)).is_none(),
            "a member: all done, the card goes"
        );
    }

    #[test]
    fn step_five_asks_for_game_activity_when_it_is_off() {
        let none = |_: &str| false;
        let mut inp = input("own", &none);
        inp.activity_on = false;
        let c = build(&quests("own"), &inp).expect("card");
        assert_eq!(c.rows[4].meta, "Game activity is off");
        assert_eq!(c.rows[4].action, "settings");
    }

    #[test]
    fn open_week_and_plus_show_their_text() {
        let all = |_: &str| true;
        let mut q = quests("own");
        q.steps.clip = Some(Default::default());
        q.steps.session = Some(Default::default());
        // Monday 2 November 2026 is in ISO week 45.
        q.gifts.founding_weeks = vec![1_793_577_600_000];
        let c = build(&q, &input("own", &all)).expect("steps 2 and 3 are not done");
        assert_eq!(c.gifts[3].text, "W45");
        assert_eq!(c.gifts[4].text, "30 DAYS");
    }

    #[test]
    fn a_ready_gift_of_the_owner_carries_its_face() {
        let none = |_: &str| false;
        let mut q = quests("own");
        q.steps.friend = Some(QuestFriend {
            user_id: "ida".into(),
            name: "Ida".into(),
            invited_by: "own".into(),
            invited_by_name: "b0bben".into(),
            ..Default::default()
        });
        let c = build(&q, &input("own", &none)).expect("card");
        let made = &c.gifts[0].reveal;
        assert_eq!(made.label, "QUEST  ·  CREW MADE");
        assert_eq!(made.moment_title, "bob_test_26");
        assert_eq!(made.moment_sub, "Made 5 November");
        assert_eq!(made.name, "Crew banner");
        assert_eq!(
            (made.action1.as_str(), made.action2.as_str()),
            ("banner-game", "banner-upload")
        );
        assert!(!made.bright.is_empty(), "the back is the field of the tile");
        let friend = &c.gifts[1].reveal;
        assert_eq!(friend.moment_title, "Ida joined");
        assert_eq!(friend.moment_sub, "Invited by you");
        assert_eq!(
            friend.gift_text, "bobtest26",
            "a link name made from the crew name"
        );
        assert_eq!(friend.action1, "link");
        assert_eq!(friend.count, "2 / 5");
        assert!(
            c.gifts[2].reveal.step.is_empty(),
            "a locked gift has no face"
        );
    }

    #[test]
    fn a_member_gets_no_face() {
        let none = |_: &str| false;
        let c = build(&quests("own"), &input("ida", &none)).expect("card");
        assert!(c.gifts.iter().all(|g| g.reveal.step.is_empty()));
    }

    #[test]
    fn iso_weeks() {
        // 2026-11-02 (Mon) and 2026-11-08 (Sun) are week 45; 2021-01-03 is
        // week 53 of 2020; 2024-12-30 is week 1 of 2025.
        assert_eq!(iso_week(1_793_577_600_000), 45);
        assert_eq!(iso_week(1_794_096_000_000), 45);
        assert_eq!(iso_week(1_609_632_000_000), 53);
        assert_eq!(iso_week(1_735_516_800_000), 1);
    }

    #[test]
    fn a_call_with_a_guest_says_so() {
        let none = |_: &str| false;
        let mut q = quests("own");
        q.steps.call = Some(QuestCall {
            user_ids: vec!["own".into()],
            names: vec!["b0bben".into()],
            guests: 1,
            ..Default::default()
        });
        let c = build(&q, &input("own", &none)).expect("card");
        assert_eq!(c.rows[2].meta, "You, +1 guest");
    }
}
