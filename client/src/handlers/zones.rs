//! The zone feed.
//!
//! The feed is five fixed zones and Crew history: Stage, Board, Clips,
//! People, Sessions. The server picks what each zone holds (`crew_feed`,
//! `FeedZones`); this module draws it into one `ZonesData` and adds what is
//! client state: a live stream on the Stage, live sessions, voice, the sealed
//! state of the recap and the unread order. A zone with nothing to show
//! collapses.
//!
//! Previews: `client/ui/previews/feed_board_preview.slint`.

use std::rc::Rc;

use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, VecModel};

use crate::app_context::AppContext;
use crate::{
    BoardGame, BoardStat, BoardTitle, FeedCardData, H2hPerson, HistoryWeek, LockedTeaser, ZonesData,
};

/// Belt rows on the board before "+N more".
const BOARD_BELTS: usize = 7;
/// The octagons of the People zone: the first five of a crew.
const FIRST_FIVE: usize = 5;
/// People in the voice part before "+N".
const VOICE_PEOPLE: usize = 5;
/// Locked teasers in a week row before "+N".
const TEASERS: usize = 2;

/// The quest card of the active crew, or `None` when it goes
/// (`quests::build`). Reads the client state the card needs.
fn quest_card(
    ctx: &AppContext,
    q: &mello_core::crew_events::ZoneQuests,
) -> Option<super::quests::Card> {
    let crew = ctx.app.get_active_crew_id().to_string();
    let settings = ctx.settings.borrow();
    let opened = crate::sealed::opened_keys(&settings, &crew);
    let is_open = |k: &str| opened.contains(k);
    let clip_key = crate::callbacks::settings::game_clip_key_label(&settings);
    let activity_on = settings.share_game_activity;
    drop(settings);
    let (viewer, members) = super::clip::h2h_roster(ctx);
    let person = |id: &str| -> Option<H2hPerson> {
        std::iter::once(&viewer)
            .chain(members.iter())
            .find(|p| p.id == id)
            .map(|p| super::clip::live_person(ctx, p))
    };
    let banner_art = (!q.gifts.banner_game_id.is_empty())
        .then(|| {
            crate::game_icons::resolve_art_worker(
                ctx,
                std::slice::from_ref(&q.gifts.banner_game_id),
                mello_core::art::ArtSlot::Hero,
            )
        })
        .flatten();
    let crew_name = ctx.app.get_active_crew_name().to_string();
    super::quests::build(
        q,
        &super::quests::Input {
            crew_id: &crew,
            crew_name: &crew_name,
            viewer_id: viewer.id.as_str(),
            opened: &is_open,
            clip_key: &clip_key,
            activity_on,
            person: &person,
            banner_art,
            // The loader for media images comes with the gift screens.
            emoji_art: None,
        },
    )
}

/// Build the zones again from the last feed of the active crew and the client
/// state now. A poster that is on screen stays on its card (`keep_posters`).
pub(crate) fn refresh(ctx: &AppContext) {
    let active = ctx.app.get_active_crew_id();
    let zones = ctx
        .last_feed
        .borrow()
        .as_ref()
        .filter(|f| f.crew_id == active.as_str())
        .map(|f| f.zones.clone());
    // Before the feed loads the zones still draw the client state: a live
    // stream, live sessions, voice.
    let mut z = build_from_zones(ctx, &zones.unwrap_or_default());
    keep_posters(&ctx.app.get_feed_zones(), &mut z);
    log::debug!(
        "feed zones: stage={} board={} clips={} people={} sessions={} history={} plus={}",
        z.stage,
        z.board,
        z.clips.row_count(),
        z.people,
        z.sessions.row_count(),
        z.history.row_count(),
        z.plus
    );
    ctx.app.set_feed_zones(z);
}

/// Where a card of the zones is: its model and row. `None` for the Stage and
/// the recap, which are not in a model.
pub(crate) type CardAt = Option<(ModelRc<FeedCardData>, usize)>;

/// Every card of the zones, with where it is.
pub(crate) fn cards(z: &ZonesData) -> Vec<(CardAt, FeedCardData)> {
    let mut out = vec![(None, z.stage_card.clone()), (None, z.recap_card.clone())];
    let mut add = |m: &ModelRc<FeedCardData>| {
        for i in 0..m.row_count() {
            if let Some(c) = m.row_data(i) {
                out.push((Some((m.clone(), i)), c));
            }
        }
    };
    add(&z.clips);
    add(&z.sessions);
    for w in z.history.iter() {
        add(&w.clips);
    }
    out.retain(|(_, c)| !c.id.is_empty());
    out
}

/// Change every card with this id, in its model or on the Stage or the
/// board. The same entry can be on the Stage and in the sessions.
pub(crate) fn update_cards(app: &crate::MainWindow, id: &str, f: impl Fn(&mut FeedCardData)) {
    let mut z = app.get_feed_zones();
    let mut whole = false;
    for (at, mut c) in cards(&z) {
        if c.id.as_str() != id {
            continue;
        }
        f(&mut c);
        match at {
            Some((m, i)) => m.set_row_data(i, c),
            None => whole = true,
        }
    }
    if whole {
        if z.stage_card.id.as_str() == id {
            f(&mut z.stage_card);
        }
        if z.recap_card.id.as_str() == id {
            f(&mut z.recap_card);
        }
        app.set_feed_zones(z);
    }
}

/// A rebuild makes new cards. A card that held a poster, a playback frame or
/// an error keeps it: the instance on screen may not ask again.
fn keep_posters(old: &ZonesData, new: &mut ZonesData) {
    let held: std::collections::HashMap<SharedString, FeedCardData> = cards(old)
        .into_iter()
        .map(|(_, c)| c)
        .filter(|c| c.snapshot_poster_ready || c.snapshot_error || c.snapshot_playback_revision > 0)
        .map(|c| (c.id.clone(), c))
        .collect();
    if held.is_empty() {
        return;
    }
    let carry = |c: &mut FeedCardData| {
        if let Some(o) = held.get(&c.id) {
            c.snapshot_loading = o.snapshot_loading;
            c.snapshot_poster = o.snapshot_poster.clone();
            c.snapshot_poster_ready = o.snapshot_poster_ready;
            c.snapshot_error = o.snapshot_error;
            c.snapshot_playback_frame = o.snapshot_playback_frame.clone();
            c.snapshot_playback_index = o.snapshot_playback_index;
            c.snapshot_playback_revision = o.snapshot_playback_revision;
        }
    };
    carry(&mut new.stage_card);
    carry(&mut new.recap_card);
    for (at, mut c) in cards(new) {
        if let Some((m, i)) = at {
            if held.contains_key(&c.id) {
                carry(&mut c);
                m.set_row_data(i, c);
            }
        }
    }
}

fn rows<T: Clone + 'static>(m: &ModelRc<T>) -> Vec<T> {
    (0..m.row_count()).filter_map(|i| m.row_data(i)).collect()
}

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    Rc::new(VecModel::from(v)).into()
}

fn stat(value: &str, unit: &str, tail: &str, label: &str) -> BoardStat {
    BoardStat {
        value: value.into(),
        unit: unit.into(),
        tail: tail.into(),
        label: label.into(),
    }
}

/// "6 h 40" from 400 minutes; "48 min" under an hour.
fn minutes_stat(min: i64, label: &str) -> BoardStat {
    let min = min.max(0);
    if min >= 60 {
        stat(
            &(min / 60).to_string(),
            "h",
            &format!("{:02}", min % 60),
            label,
        )
    } else {
        stat(&min.to_string(), "min", "", label)
    }
}

fn count_stat(n: i64, label: &str) -> BoardStat {
    stat(&n.max(0).to_string(), "", "", label)
}

/// "25 h 34" as text, for a person line.
fn minutes_text(min: i64) -> String {
    let min = min.max(0);
    if min >= 60 {
        format!("{} h {:02}", min / 60, min % 60)
    } else {
        format!("{min} min")
    }
}

/// The art of a card's game: the header capsule when there is one.
fn card_art(c: &FeedCardData) -> Image {
    if c.game_has_capsule {
        c.game_capsule.clone()
    } else {
        c.game_icon.clone()
    }
}

fn names_line(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [a] => a.clone(),
        [a, b] => format!("{a} and {b}"),
        [a, b, c] => format!("{a}, {b} and {c}"),
        [a, b, rest @ ..] => format!("{a}, {b} and {} more", rest.len()),
    }
}

/// The zones from the server's zones payload (`crew_feed`).
/// The client adds what is client state: a live stream on the Stage, live
/// sessions, voice, the sealed state of the recap and the unread order.
fn build_from_zones(ctx: &AppContext, zs: &mello_core::crew_events::FeedZones) -> ZonesData {
    use mello_core::crew_events::FeedEntry;
    let live = ctx.app.get_active_stream_cards().row_count() > 0;
    let now_ms = crate::converters::current_time_ms();
    let card =
        |e: &FeedEntry| super::clip::build_feed_card(ctx, &e.id, &e.entry_type, &e.data, e.ts);
    let (viewer, members) = super::clip::h2h_roster(ctx);
    let you = super::clip::live_person(ctx, &viewer);
    let mut z = ZonesData {
        you: you.clone(),
        plus: zs.plus,
        ..Default::default()
    };

    // Head2head: the crew's data (belts, seats) and the belt hero.
    let (belt_hero, h2h_data) = match zs
        .head2head
        .as_ref()
        .and_then(|d| super::clip::head2head_zone(ctx, d))
    {
        Some((hero, data)) => (hero, Some(data)),
        None => (None, None),
    };

    // The viewer's own chips (a crew of one), for the Stage or, while the
    // quest card holds the Stage, the solo Board.
    let own_chips = || -> Vec<crate::TitleHeld> {
        h2h_data
            .as_ref()
            .map(|d| rows(&d.chips))
            .unwrap_or_default()
            .into_iter()
            .filter(|t| !t.belt.key.is_empty())
            .take(4)
            .collect()
    };
    let quest = zs.quests.as_ref().and_then(|q| quest_card(ctx, q));

    // ── Stage. A live stream comes first; it is client state. Then the
    // quest card of a new crew, which the server sends beside its Stage.
    let mut stage_id = String::new();
    if live {
        z.stage = "live".into();
    } else if let Some(card) = quest {
        z.stage = "quests".into();
        z.quest_count = card.count.into();
        z.quest_fill = card.fill;
        z.quest_gifts = model(card.gifts);
        z.quest_rows = model(card.rows);
        if zs.stage.kind == "own_belts" {
            z.own_chips = model(own_chips());
        }
    } else {
        match zs.stage.kind.as_str() {
            "game_clip" | "stream_replay" => {
                if let Some(e) = &zs.stage.entry {
                    let mut c = card(e);
                    c.is_hero = true;
                    stage_id = e.id.clone();
                    z.stage_card = c;
                    z.stage = "card".into();
                }
            }
            "belt" => {
                // A belt that is sealed on the board keeps its holder hidden:
                // the Stage shows it only once it is open.
                let sealed = |c: &FeedCardData| {
                    let id = c.head2head.belt.title_id.as_str();
                    h2h_data.as_ref().is_some_and(|d| {
                        rows(&d.titles)
                            .iter()
                            .any(|t| t.belt.sealed && t.belt.key.split(':').next() == Some(id))
                    })
                };
                if let Some(c) = belt_hero.as_ref().filter(|c| !sealed(c)) {
                    z.stage_card = c.clone();
                    z.stage_short = true;
                    z.stage = "card".into();
                }
            }
            "own_belts" => {
                let chips = own_chips();
                if !chips.is_empty() {
                    z.stage = "own".into();
                    z.own_chips = model(chips);
                }
            }
            _ => {}
        }
    }

    // ── Board. The recap shows sealed until the viewer opened it; while
    // something is live the feed is not quiet and not dormant.
    let recap = zs.board.recap.as_ref().map(card);
    let sealed = recap
        .as_ref()
        .is_some_and(|r| r.recap_seal.sealed && !r.recap_seal.key.is_empty());
    let mode = zs.board.mode.as_str();
    z.board = if sealed {
        "sealed"
    } else if mode.is_empty() || (live && (mode == "quiet" || mode == "past")) {
        "tabs"
    } else {
        mode
    }
    .into();
    match &recap {
        Some(r) => {
            z.recap_card = r.clone();
            week_numbers(&mut z, r);
        }
        None => z.week_big = minutes_stat(0, "HANGOUT"),
    }
    match &zs.board.today {
        Some(e) => {
            let n = |k: &str| e.data.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
            let on = e
                .data
                .get("member_ids")
                .and_then(|v| v.as_array())
                .map_or(0, |a| a.len() as i64);
            z.today_big = minutes_stat(n("total_min"), "HANGOUT");
            z.today_stats = model(vec![
                count_stat(n("clip_count"), "CLIPS"),
                count_stat(n("session_count"), "SESSIONS"),
                count_stat(on, "ON"),
            ]);
            let day = card(e);
            z.today_has_top = !day.game_name.is_empty();
            z.today_top = BoardGame {
                name: day.game_name.clone(),
                art: card_art(&day),
                count: SharedString::new(),
            };
        }
        None => {
            z.today_empty = true;
            z.today_big = minutes_stat(0, "HANGOUT");
        }
    }
    if z.board == "quiet" {
        z.quiet_line = "No crew time last week.".into();
        if !zs.board.players_last_week.is_empty() {
            z.quiet_sub =
                format!("{} still played.", names_line(&zs.board.players_last_week)).into();
        }
    }
    if let Some(d) = &h2h_data {
        board_belts(ctx, &mut z, d, &viewer, &you, &members);
    }
    if z.board == "past" && zs.board.last_activity_ms > 0 {
        let last = zs.board.last_activity_ms;
        let mut line = format!("Nothing new since {}.", crate::feed_age::date_label(last));
        if let Some(t) = z.titles.row_data(0) {
            if !t.belt.sealed {
                line.push_str(&format!(
                    " {} holds {}.",
                    t.person.name,
                    title_case(&t.title)
                ));
            }
        }
        z.dormant_line = line.into();
        let days = crate::feed_age::days_between(last, now_ms);
        z.dormant_age = format!("{} weeks ago", (days / 7).max(2)).into();
    }

    // ── Clips: the server's order (game first, then newest), unread first.
    let mut clips: Vec<FeedCardData> = zs
        .clips
        .iter()
        .filter(|e| e.id != stage_id)
        .map(card)
        .collect();
    clips.sort_by_key(|c| !c.is_new);
    z.clips = model(clips);

    // ── People.
    z.open_seats = zs.people.open_seats;
    // The first five: the viewer, then the crew in roster order.
    let mut first_five = vec![you.clone()];
    first_five.extend(
        members
            .iter()
            .filter(|m| m.id != viewer.id)
            .take(FIRST_FIVE - 1)
            .map(|m| super::clip::live_person(ctx, m)),
    );
    z.members = model(first_five);
    people_joined(&mut z, &super::clip::join_cards(ctx, &zs.people.joined));
    let last_call = zs
        .sessions
        .iter()
        .filter(|e| e.data.get("channel_id").is_some() || e.data.get("channel_name").is_some())
        .map(|e| e.ts)
        .max()
        .map(|t| format!("Last call {}", crate::feed_age::date_label(t)))
        .unwrap_or_default();
    people_voice(ctx, &mut z, &last_call);
    z.people = z.open_seats > 0 || z.joined.row_count() > 0 || !z.voice.is_empty();

    // ── Sessions: live sessions (client state) first, then the server's.
    let mut sessions = super::clip::live_session_cards(ctx);
    sessions.extend(zs.sessions.iter().map(card));
    z.sessions = model(sessions);
    if !zs.installed.is_empty() {
        let tiles = super::clip::extract_recent_game_tiles(
            ctx,
            &serde_json::json!({ "games": zs.installed }),
        );
        let art = |t: &crate::RecentGameTile| {
            if t.has_capsule {
                t.capsule.clone()
            } else {
                t.game_icon.clone()
            }
        };
        if z.board == "solo" {
            z.solo_games = model(
                tiles
                    .iter()
                    .take(4)
                    .map(|t| BoardGame {
                        name: t.game_name.clone(),
                        art: art(t),
                        count: t.hours.to_lowercase().into(),
                    })
                    .collect(),
            );
        } else if z.sessions.row_count() == 0 {
            z.installed = model(
                tiles
                    .iter()
                    .take(3)
                    .map(|t| BoardGame {
                        name: t.game_name.clone(),
                        art: art(t),
                        count: t.owners.row_count().to_string().into(),
                    })
                    .collect(),
            );
        }
    }

    // ── Crew history: the pages loaded so far.
    let person = |id: &str, name: &str| -> H2hPerson {
        match members.iter().find(|m| m.id == id) {
            Some(m) => super::clip::live_person(ctx, m),
            None => H2hPerson {
                id: id.into(),
                name: name.into(),
                initials: name
                    .chars()
                    .take(2)
                    .collect::<String>()
                    .to_uppercase()
                    .into(),
                ..Default::default()
            },
        }
    };
    z.history = model(
        zs.history
            .iter()
            .map(|w| {
                let mut h = HistoryWeek {
                    label: if w.week_number > 0 {
                        format!("Week {}", w.week_number).into()
                    } else {
                        crate::feed_age::week_of_label(w.week_start).into()
                    },
                    age: crate::feed_age::recap_age_label(w.week_start, now_ms).into(),
                    locked: w.locked,
                    ..Default::default()
                };
                if w.locked {
                    h.seal = locked_seal(w.week_start);
                    h.teasers = model(
                        w.teasers
                            .iter()
                            .take(TEASERS)
                            .map(|t| LockedTeaser {
                                person: person(&t.actor_id, &t.actor_name),
                                kind: t.kind.to_uppercase().into(),
                                duration: teaser_length(&t.kind, t.duration_seconds).into(),
                                code: crate::sealed::cells_path(&crate::sealed::belt_cells(
                                    crate::sealed::recap_seed(t.ts),
                                ))
                                .into(),
                            })
                            .collect(),
                    );
                    let total = (w.clip_count + w.stream_count).max(0) as usize;
                    h.more = total.saturating_sub(TEASERS) as i32;
                } else {
                    if let Some(r) = w.recap.as_ref().map(card) {
                        let mut tmp = ZonesData::default();
                        week_numbers(&mut tmp, &r);
                        h.big = tmp.week_big;
                        h.stats = tmp.week_stats;
                        h.has_top = tmp.week_has_top;
                        h.top = tmp.week_top;
                    } else {
                        h.big = count_stat(
                            i64::from(w.clip_count + w.stream_count),
                            "CLIPS AND STREAMS",
                        );
                    }
                    h.clips = model(w.items.iter().map(card).collect());
                }
                h
            })
            .collect(),
    );
    z.history_more = zs.history_next > 0;
    z
}

/// The numbers of a recap week: hangout, clips, longest, games, top game,
/// most active, most clipped and the games played.
fn week_numbers(z: &mut ZonesData, r: &FeedCardData) {
    z.week_big = minutes_stat(i64::from(r.recap_hangout_min), "HANGOUT");
    let games = rows(&r.recap_games);
    z.week_stats = model(vec![
        count_stat(i64::from(r.recap_clips), "CLIPS"),
        minutes_stat(i64::from(r.recap_longest_min), "LONGEST"),
        count_stat(games.len() as i64, "GAMES"),
    ]);
    z.week_has_top = !r.game_name.is_empty();
    z.week_top = BoardGame {
        name: r.game_name.clone(),
        art: card_art(r),
        count: SharedString::new(),
    };
    if !r.recap_active.name.is_empty() {
        z.most_active = person_of_recap(&r.recap_active);
        z.most_active_value = minutes_text(i64::from(r.recap_active.count)).into();
    }
    if !r.recap_clipped.name.is_empty() {
        z.most_clipped = person_of_recap(&r.recap_clipped);
        z.most_clipped_value = r.recap_clipped.count.to_string().into();
    }
    z.games_played = model(
        games
            .iter()
            .take(3)
            .map(|g| {
                if g.has_capsule {
                    g.capsule.clone()
                } else {
                    g.icon.clone()
                }
            })
            .collect(),
    );
    z.games_more = games.len().saturating_sub(3) as i32;
    z.past_label = r.recap_week.to_uppercase().into();
    z.past_age = r.age_label.clone();
}

/// The sealed look of a locked week: the recap field of the week, its own key.
fn locked_seal(week_start: i64) -> crate::RecapSeal {
    let seed = crate::sealed::recap_seed(week_start);
    let levels = crate::sealed::recap_levels(seed);
    let (bright, dim) = crate::sealed::recap_field(&levels);
    crate::RecapSeal {
        key: format!("plus:{week_start}").into(),
        sealed: true,
        seed,
        bright: bright.into(),
        dim: dim.into(),
        levels: model(levels),
    }
}

/// "0:30" for a clip, "1 h 04" or "48 min" for a stream.
fn teaser_length(kind: &str, seconds: f64) -> String {
    let secs = seconds.max(0.0).round() as i64;
    if kind == "stream" {
        minutes_text(secs / 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// "NIGHT OWL" as "Night Owl".
fn title_case(s: &str) -> String {
    s.split(' ')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Wire the zone feed's history paging. The callback holds the window weakly,
/// like every other callback, so the window can drop.
pub(crate) fn wire_history_paging(ctx: &AppContext) {
    let app = ctx.app.as_weak();
    let last_feed = ctx.last_feed.clone();
    let pending = ctx.zones_history_pending.clone();
    let cmd_tx = ctx.cmd_tx.clone();
    ctx.app.on_zones_history_more(move || {
        let Some(app) = app.upgrade() else {
            return;
        };
        let next = last_feed
            .borrow()
            .as_ref()
            .map_or(0, |f| f.zones.history_next);
        let crew = app.get_active_crew_id().to_string();
        if next <= 0 || crew.is_empty() || pending.get() == next {
            return;
        }
        pending.set(next);
        log::info!("feed zones: loading Crew history before {next}");
        let _ = cmd_tx.send(mello_core::Command::LoadCrewHistory {
            crew_id: crew,
            before: next,
        });
    });
}

/// A page of Crew history arrived: append its weeks, keep the rest of the feed.
pub(crate) fn append_history(ctx: &AppContext, response: mello_core::crew_events::FeedResponse) {
    ctx.zones_history_pending.set(0);
    let page = response.zones;
    {
        let mut last = ctx.last_feed.borrow_mut();
        let Some(feed) = last.as_mut().filter(|f| f.crew_id == response.crew_id) else {
            return;
        };
        let zones = &mut feed.zones;
        let known: std::collections::HashSet<i64> =
            zones.history.iter().map(|w| w.week_start).collect();
        zones.history.extend(
            page.history
                .into_iter()
                .filter(|w| !known.contains(&w.week_start)),
        );
        zones.history_next = page.history_next;
    }
    refresh(ctx);
}

/// The belts of the board: the crew's titles, a moved or sealed belt first.
fn board_belts(
    ctx: &AppContext,
    z: &mut ZonesData,
    d: &crate::Head2headData,
    viewer: &crate::head2head::Person,
    you: &H2hPerson,
    members: &[crate::head2head::Person],
) {
    let person = |id: &str| -> H2hPerson {
        if id == viewer.id {
            let mut p = you.clone();
            p.name = "You".into();
            return p;
        }
        if let Some(m) = rows(&d.members).into_iter().find(|m| m.id == id) {
            return m;
        }
        match members.iter().find(|m| m.id == id) {
            Some(m) => super::clip::live_person(ctx, m),
            None => H2hPerson {
                id: id.into(),
                name: "Crew mate".into(),
                ..Default::default()
            },
        }
    };
    let mut titles = rows(&d.titles);
    titles.sort_by_key(|t| !(t.belt.sealed || t.moved_recent));
    z.title_count = titles.len() as i32;
    if titles.len() > BOARD_BELTS {
        z.more_label = format!("+{} MORE", titles.len() - BOARD_BELTS).into();
    }
    z.titles = model(
        titles
            .iter()
            .take(BOARD_BELTS)
            .map(|t| BoardTitle {
                person: person(t.holder_id.as_str()),
                title: t.title.to_uppercase().into(),
                icon: t.icon.clone(),
                value: t.value.clone(),
                unit: t.unit.clone(),
                tail: SharedString::new(),
                unread: t.moved_recent,
                belt: t.belt.clone(),
            })
            .collect(),
    );
}

/// Who joined in the last 24 hours, from the joined cards.
fn people_joined(z: &mut ZonesData, joined: &[FeedCardData]) {
    if !joined.is_empty() {
        let names: Vec<String> = joined.iter().map(|c| c.actor_name.to_string()).collect();
        z.joined_line = format!("{} joined", names_line(&names)).into();
        let first = &joined[0];
        z.joined_sub = [first.subtitle.to_string(), first.timestamp.to_string()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", ")
            .into();
        z.joined = model(
            joined
                .iter()
                .map(|c| H2hPerson {
                    id: c.id.clone(),
                    name: c.actor_name.clone(),
                    initials: c.actor_initials.clone(),
                    hue_index: c.actor_hue_index,
                    avatar: c.actor_avatar.clone(),
                    has_avatar: c.actor_has_avatar,
                })
                .collect(),
        );
        if let Some(c) = joined.iter().find(|c| !c.said_hi) {
            z.say_hi_card = c.id.clone();
            z.say_hi_name = c.actor_name.clone();
        } else {
            z.said_hi = true;
        }
    }
}

/// The voice part of People: a call in the default channel, or the empty
/// channel on a quiet feed. `last_call` is "Last call 6 November" or empty.
fn people_voice(ctx: &AppContext, z: &mut ZonesData, last_call: &str) {
    let channels = rows(&ctx.app.get_voice_channels());
    if let Some(ch) = channels.iter().find(|c| c.is_default) {
        z.channel = ch.name.clone();
        z.channel_id = ch.id.clone();
        if ch.member_count > 0 {
            let people = rows(&ch.members);
            z.voice = "call".into();
            z.voice_line = format!("{} in the call", ch.member_count).into();
            z.voice_more = (people.len().saturating_sub(VOICE_PEOPLE)) as i32;
            z.voice_people = model(
                people
                    .iter()
                    .take(VOICE_PEOPLE)
                    .map(|m| H2hPerson {
                        id: m.id.clone(),
                        name: m.name.clone(),
                        initials: m.initials.clone(),
                        hue_index: 0,
                        avatar: m.avatar.clone(),
                        has_avatar: m.has_avatar,
                    })
                    .collect(),
            );
        } else if z.board == "quiet" || z.board == "past" || z.stage.is_empty() {
            let last = last_call;
            z.voice = "idle".into();
            z.voice_line = if last.is_empty() {
                "Empty".into()
            } else {
                format!("Empty. {last}").into()
            };
        }
    }
}

fn person_of_recap(p: &crate::RecapPerson) -> H2hPerson {
    H2hPerson {
        id: SharedString::new(),
        name: p.name.clone(),
        initials: p.initials.clone(),
        hue_index: p.hue,
        avatar: p.avatar.clone(),
        has_avatar: p.has_avatar,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minutes_read_as_hours_and_minutes() {
        let s = minutes_stat(400, "HANGOUT");
        assert_eq!(
            (s.value.as_str(), s.unit.as_str(), s.tail.as_str()),
            ("6", "h", "40")
        );
        let s = minutes_stat(48, "HANGOUT");
        assert_eq!(
            (s.value.as_str(), s.unit.as_str(), s.tail.as_str()),
            ("48", "min", "")
        );
        assert_eq!(minutes_text(1534), "25 h 34");
    }

    #[test]
    fn names_line_counts_the_rest() {
        let n = |v: &[&str]| names_line(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(n(&["Elin"]), "Elin");
        assert_eq!(n(&["Elin", "Noah", "Saga"]), "Elin, Noah and Saga");
        assert_eq!(n(&["A", "B", "C", "D"]), "A, B and 2 more");
    }
}
