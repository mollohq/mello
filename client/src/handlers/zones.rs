//! The zone feed (prototype, `MELLO_FEED_ZONES=1`).
//!
//! The feed is five fixed zones: Stage, Board, Clips, People, Games. A zone
//! with nothing to show collapses. This module folds the feed the client
//! already has (the built cards, the raw `this_week` entries, live streams,
//! voice channels, the quiet and dormant state) into one `ZonesData`. The
//! server still sends the card list; nothing here changes it.
//!
//! Previews: `client/ui/previews/feed_board_preview.slint` (`PreviewZones*`).

use std::rc::Rc;

use slint::{Image, Model, ModelRc, SharedString, VecModel};

use crate::app_context::AppContext;
use crate::{
    BoardGame, BoardSession, BoardStat, BoardTitle, FeedCardData, H2hPerson, HistoryWeek,
    LockedTeaser, ZonesData,
};

/// Belt rows on the board before "+N more".
const BOARD_BELTS: usize = 7;
/// Session rows before "+N more".
const SESSION_ROWS: usize = 3;
/// People in the voice part before "+N".
const VOICE_PEOPLE: usize = 5;
/// Free is the last 7 days. Every week before it is m3llo+.
const FREE_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// Locked teasers in a week row before "+N".
const TEASERS: usize = 2;

/// The crew has m3llo+ for this run (prototype; the server does not gate yet).
pub(crate) fn plus_from_env() -> bool {
    std::env::var("MELLO_PLUS").is_ok_and(|v| v == "1")
}

/// The zone feed is on for this run.
pub(crate) fn enabled_from_env() -> bool {
    std::env::var("MELLO_FEED_ZONES").is_ok_and(|v| v == "1")
}

/// Build the zones again from what the client holds now. A no-op while the
/// zone feed is off.
pub(crate) fn refresh(ctx: &AppContext) {
    if !ctx.app.get_feed_zones_on() {
        return;
    }
    let z = build(ctx);
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

fn is_game_clip(c: &FeedCardData) -> bool {
    c.card_type == "clip" && c.clip_kind == "game"
}

fn build(ctx: &AppContext) -> ZonesData {
    let cards = rows(&ctx.app.get_feed_cards());
    let memory = rows(&ctx.app.get_memory_cards());
    let live = ctx.app.get_active_stream_cards().row_count() > 0;
    let quiet = rows(&ctx.app.get_feed_quiet());
    let dormant = rows(&ctx.app.get_feed_dormant());

    let entries: Vec<mello_core::crew_events::FeedEntry> = ctx
        .last_feed
        .borrow()
        .as_ref()
        .and_then(|f| f.sections.iter().find(|s| s.id == "this_week"))
        .map(|s| s.entries.clone())
        .unwrap_or_default();
    let memory_entries: Vec<mello_core::crew_events::FeedEntry> = ctx
        .last_feed
        .borrow()
        .as_ref()
        .and_then(|f| f.sections.iter().find(|s| s.id == "memory"))
        .map(|s| s.entries.clone())
        .unwrap_or_default();
    // The time of every card, by id: the built cards carry only labels.
    let ts_of: std::collections::HashMap<String, i64> = entries
        .iter()
        .chain(memory_entries.iter())
        .map(|e| (e.id.clone(), e.ts))
        .collect();
    let now_ms = crate::converters::current_time_ms();
    let is_free = |id: &str| {
        ts_of
            .get(id)
            .is_none_or(|&t| t <= 0 || now_ms - t <= FREE_MS)
    };

    // Head2head: the crew's titles, members and seats, and the belt hero card.
    let h2h = entries
        .iter()
        .find(|e| e.entry_type == "head2head")
        .and_then(|e| super::clip::head2head_cards(ctx, e, &entries));
    let (h2h_cards, h2h_data) = match h2h {
        Some((_, cards, data)) => (cards, Some(data)),
        None => (Vec::new(), None),
    };
    let solo = h2h_data.as_ref().is_some_and(|d| d.variant == "wide");

    let (viewer, members) = super::clip::h2h_roster(ctx);
    let you = super::clip::live_person(ctx, &viewer);

    let mut z = ZonesData {
        you: you.clone(),
        ..Default::default()
    };

    // ── Stage: live streams, then game clips, then stream replays, then the
    // belt hero of a quiet feed, then a crew of one's own belts.
    let belt_card = h2h_cards
        .iter()
        .find(|c| c.card_type == "head2head" && c.head2head.variant == "belt")
        .cloned();
    let stage_card = if live {
        None
    } else {
        cards
            .iter()
            .find(|c| c.is_hero && (is_game_clip(c) || c.card_type == "session-preview"))
            .or_else(|| cards.iter().find(|c| is_game_clip(c)))
            .or_else(|| cards.iter().find(|c| c.card_type == "session-preview"))
            .cloned()
            .map(|mut c| {
                c.is_hero = true;
                c
            })
            .or(belt_card.clone())
            .or_else(|| {
                memory
                    .iter()
                    .find(|c| c.card_type == "session-preview")
                    .cloned()
                    .map(|mut c| {
                        c.is_hero = true;
                        c
                    })
            })
    };
    let stage_id = stage_card.as_ref().map(|c| c.id.to_string());
    if live {
        z.stage = "live".into();
    } else if let Some(c) = stage_card {
        z.stage_short = c.card_type == "head2head";
        z.stage_card = c;
        z.stage = "card".into();
    } else if solo {
        let belts: Vec<crate::BeltReveal> = h2h_data
            .as_ref()
            .map(|d| rows(&d.chips))
            .unwrap_or_default()
            .into_iter()
            .map(|t| t.belt)
            .filter(|b| !b.key.is_empty())
            .take(4)
            .collect();
        if !belts.is_empty() {
            z.stage = "own".into();
            z.own_belts = model(belts);
        }
    }

    // ── Board.
    let recap = cards
        .iter()
        .find(|c| c.card_type == "recap")
        .or_else(|| memory.iter().find(|c| c.card_type == "recap"))
        .cloned();
    let recap_sealed = recap
        .as_ref()
        .is_some_and(|r| r.recap_seal.sealed && !r.recap_seal.key.is_empty());
    z.board = if recap_sealed && cards.iter().any(|c| c.card_type == "recap") {
        "sealed"
    } else if !dormant.is_empty() {
        "past"
    } else if !quiet.is_empty() {
        "quiet"
    } else if solo {
        "solo"
    } else {
        "tabs"
    }
    .into();
    if let Some(r) = &recap {
        z.recap_card = r.clone();
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
    } else {
        z.week_big = minutes_stat(0, "HANGOUT");
    }

    // Today: the newest daily recap, when it is today.
    let now = crate::converters::current_time_ms();
    let today = entries.iter().find(|e| {
        e.entry_type == "daily-recap"
            && e.data
                .get("day_start")
                .and_then(|v| v.as_i64())
                .is_some_and(|d| crate::feed_age::day_label(d, now) == "Today so far")
    });
    match today {
        Some(e) => {
            let d = &e.data;
            let n = |k: &str| d.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
            let on = d
                .get("member_ids")
                .and_then(|v| v.as_array())
                .map_or(0, |a| a.len() as i64);
            z.today_big = minutes_stat(n("total_min"), "HANGOUT");
            z.today_stats = model(vec![
                count_stat(n("clip_count"), "CLIPS"),
                count_stat(n("session_count"), "SESSIONS"),
                count_stat(on, "ON"),
            ]);
            if let Some(day) = cards.iter().find(|c| c.card_type == "daily-recap") {
                z.today_has_top = !day.game_name.is_empty();
                z.today_top = BoardGame {
                    name: day.game_name.clone(),
                    art: card_art(day),
                    count: SharedString::new(),
                };
            }
        }
        None => {
            z.today_empty = true;
            z.today_big = minutes_stat(0, "HANGOUT");
        }
    }

    if let Some(q) = quiet.first() {
        let line = q.line.to_string();
        match line.split_once(". ") {
            Some((a, b)) => {
                z.quiet_line = format!("{a}.").into();
                z.quiet_sub = b.into();
            }
            None => z.quiet_line = line.into(),
        }
    }
    if let Some(d) = dormant.first() {
        z.dormant_line = d.title.clone();
        z.dormant_age = d.timestamp.clone();
    }

    // Belts: the titles of the crew, a moved or sealed belt first.
    if let Some(d) = &h2h_data {
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

    // ── Clips: every clip, game footage first, unread first.
    // The carousel holds every clip of the last 7 days: game footage first,
    // unread first, then newest. Older clips are Crew history.
    let mut clips: Vec<FeedCardData> = cards
        .iter()
        .chain(memory.iter())
        .filter(|c| c.card_type == "clip")
        .filter(|c| Some(c.id.to_string()) != stage_id)
        .filter(|c| is_free(c.id.as_str()))
        .cloned()
        .collect();
    let mut seen = std::collections::HashSet::new();
    clips.retain(|c| seen.insert(c.id.to_string()));
    let ts = |c: &FeedCardData| ts_of.get(c.id.as_str()).copied().unwrap_or(0);
    clips.sort_by_key(|c| (!is_game_clip(c), !c.is_new, std::cmp::Reverse(ts(c))));
    z.clips_more = 0;
    z.clips = model(
        clips
            .into_iter()
            .map(|mut c| {
                c.is_hero = false;
                c
            })
            .collect(),
    );

    // ── People: the first five, who joined, who is in voice.
    if let Some(d) = &h2h_data {
        z.open_seats = d.open_seats;
        let m = rows(&d.members);
        z.members = if m.is_empty() {
            model(vec![you.clone()])
        } else {
            model(m)
        };
    }
    let joined: Vec<&FeedCardData> = cards.iter().filter(|c| c.card_type == "joined").collect();
    if !joined.is_empty() {
        let names: Vec<String> = joined.iter().map(|c| c.actor_name.to_string()).collect();
        z.joined_line = format!("{} joined", names_line(&names)).into();
        let first = joined[0];
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
        } else {
            z.said_hi = true;
        }
    }
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
            let last = cards
                .iter()
                .find(|c| c.card_type == "voice-idle")
                .map(|c| c.subtitle.to_string())
                .unwrap_or_default();
            z.voice = "idle".into();
            z.voice_line = if last.is_empty() {
                "Empty".into()
            } else {
                format!("Empty. {last}").into()
            };
        }
    }
    z.people = z.open_seats > 0 || z.joined.row_count() > 0 || !z.voice.is_empty();

    // ── Games: the sessions, or what the crew has installed on a quiet week.
    let sessions: Vec<&FeedCardData> = cards
        .iter()
        .filter(|c| c.card_type == "game-session")
        .collect();
    z.sessions_count = sessions.len() as i32;
    if sessions.len() > SESSION_ROWS {
        z.sessions_more = format!("+{} MORE", sessions.len() - SESSION_ROWS).into();
    }
    z.sessions = model(
        sessions
            .iter()
            .take(SESSION_ROWS)
            .map(|c| BoardSession {
                art: card_art(c),
                game: c.game_name.clone(),
                players: if c.participants_line.is_empty() {
                    c.actor_name.clone()
                } else {
                    c.participants_line.clone()
                },
                result: c.game_record.clone(),
                length: if c.duration_human.is_empty() {
                    c.duration.clone()
                } else {
                    c.duration_human.clone()
                },
            })
            .collect(),
    );
    let tiles = cards
        .iter()
        .find(|c| c.card_type == "recent-games")
        .map(|c| rows(&c.recent_games))
        .unwrap_or_default();
    let tile_art = |t: &crate::RecentGameTile| {
        if t.has_capsule {
            t.capsule.clone()
        } else {
            t.game_icon.clone()
        }
    };
    if sessions.is_empty() && z.board == "quiet" {
        let mut by_owners = tiles.clone();
        by_owners.sort_by_key(|t| std::cmp::Reverse(t.owners.row_count()));
        z.installed = model(
            by_owners
                .iter()
                .take(3)
                .map(|t| BoardGame {
                    name: t.game_name.clone(),
                    art: tile_art(t),
                    count: t.owners.row_count().to_string().into(),
                })
                .collect(),
        );
    }
    // ── Crew history: every week before the last 7 days, newest first.
    z.plus = plus_from_env();
    z.history = model(history_weeks(
        &memory,
        &memory_entries,
        &ts_of,
        now_ms,
        z.plus,
    ));

    if solo {
        z.solo_games = model(
            tiles
                .iter()
                .take(4)
                .map(|t| BoardGame {
                    name: t.game_name.clone(),
                    art: tile_art(t),
                    count: t.hours.to_lowercase().into(),
                })
                .collect(),
        );
    }

    z
}

/// Monday 00:00 UTC of the week of `ms`.
fn week_start_of(ms: i64) -> i64 {
    const DAY: i64 = 24 * 60 * 60 * 1000;
    let day = ms.div_euclid(DAY);
    // 1970-01-01 was a Thursday: day 0 is weekday 3 when Monday is 0.
    let weekday = (day + 3).rem_euclid(7);
    (day - weekday) * DAY
}

/// The past weeks of Crew history from the memory section. A recap names its
/// week; a week without one is named by its start. Free users see every week
/// sealed with teasers; m3llo+ sees the numbers and the clips.
fn history_weeks(
    memory: &[FeedCardData],
    entries: &[mello_core::crew_events::FeedEntry],
    ts_of: &std::collections::HashMap<String, i64>,
    now_ms: i64,
    plus: bool,
) -> Vec<HistoryWeek> {
    use std::collections::BTreeMap;
    struct Week {
        recap: Option<FeedCardData>,
        items: Vec<FeedCardData>,
    }
    let mut weeks: BTreeMap<i64, Week> = BTreeMap::new();
    for c in memory {
        if c.card_type == "locked" {
            continue;
        }
        let Some(&ts) = ts_of.get(c.id.as_str()) else {
            continue;
        };
        let start = if c.card_type == "recap" {
            entries
                .iter()
                .find(|e| e.id == c.id.as_str())
                .and_then(|e| {
                    e.data
                        .get("week_start_ms")
                        .or_else(|| e.data.get("week_start"))
                        .and_then(|v| v.as_i64())
                })
                .unwrap_or_else(|| week_start_of(ts - FREE_MS))
        } else {
            if now_ms - ts <= FREE_MS {
                continue;
            }
            week_start_of(ts)
        };
        let w = weeks.entry(start).or_insert(Week {
            recap: None,
            items: Vec::new(),
        });
        if c.card_type == "recap" {
            w.recap = Some(c.clone());
        } else if matches!(c.card_type.as_str(), "clip" | "session-preview") {
            w.items.push(c.clone());
        }
    }
    weeks
        .into_iter()
        .rev()
        .filter(|(start, w)| *start > 0 && (w.recap.is_some() || !w.items.is_empty()))
        .map(|(start, mut w)| {
            w.items.sort_by_key(|c| {
                (
                    !is_game_clip(c),
                    std::cmp::Reverse(ts_of.get(c.id.as_str()).copied().unwrap_or(0)),
                )
            });
            let label = w
                .recap
                .as_ref()
                .map(|r| r.recap_week.to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| crate::feed_age::week_of_label(start));
            let mut h = HistoryWeek {
                label: label.into(),
                age: crate::feed_age::recap_age_label(start, now_ms).into(),
                locked: !plus,
                ..Default::default()
            };
            if let Some(r) = &w.recap {
                h.big = minutes_stat(i64::from(r.recap_hangout_min), "HANGOUT");
                h.stats = model(vec![
                    count_stat(i64::from(r.recap_clips), "CLIPS"),
                    minutes_stat(i64::from(r.recap_longest_min), "LONGEST"),
                    count_stat(r.recap_games.row_count() as i64, "GAMES"),
                ]);
                h.has_top = !r.game_name.is_empty();
                h.top = BoardGame {
                    name: r.game_name.clone(),
                    art: card_art(r),
                    count: SharedString::new(),
                };
            } else {
                h.big = count_stat(w.items.len() as i64, "CLIPS AND STREAMS");
            }
            if plus {
                h.clips = model(w.items);
            } else {
                // The sealed look: the recap field of this week, its own key.
                let seed = crate::sealed::recap_seed(start);
                let levels = crate::sealed::recap_levels(seed);
                let (bright, dim) = crate::sealed::recap_field(&levels);
                h.seal = crate::RecapSeal {
                    key: format!("plus:{start}").into(),
                    sealed: true,
                    seed,
                    bright: bright.into(),
                    dim: dim.into(),
                    levels: model(levels),
                };
                h.more = w.items.len().saturating_sub(TEASERS) as i32;
                h.teasers = model(
                    w.items
                        .iter()
                        .take(TEASERS)
                        .map(|c| {
                            let seed = crate::sealed::recap_seed(
                                ts_of.get(c.id.as_str()).copied().unwrap_or(start),
                            );
                            LockedTeaser {
                                person: H2hPerson {
                                    id: SharedString::new(),
                                    name: c.actor_name.clone(),
                                    initials: c.actor_initials.clone(),
                                    hue_index: c.actor_hue_index,
                                    avatar: c.actor_avatar.clone(),
                                    has_avatar: c.actor_has_avatar,
                                },
                                kind: match (c.card_type.as_str(), c.clip_kind.as_str()) {
                                    ("session-preview", _) => "STREAM",
                                    (_, "game") => "CLIP",
                                    _ => "VOICE",
                                }
                                .into(),
                                duration: if c.duration.is_empty() {
                                    c.duration_human.clone()
                                } else {
                                    c.duration.clone()
                                },
                                code: crate::sealed::cells_path(&crate::sealed::belt_cells(seed))
                                    .into(),
                            }
                        })
                        .collect(),
                );
            }
            h
        })
        .collect()
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
    fn a_week_starts_on_monday_utc() {
        // Thursday 5 November 2026, 12:00 UTC.
        let thu = 1_793_880_000_000;
        // Monday 2 November 2026, 00:00 UTC.
        assert_eq!(week_start_of(thu), 1_793_577_600_000);
        assert_eq!(week_start_of(1_793_577_600_000), 1_793_577_600_000);
    }

    #[test]
    fn names_line_counts_the_rest() {
        let n = |v: &[&str]| names_line(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(n(&["Elin"]), "Elin");
        assert_eq!(n(&["Elin", "Noah", "Saga"]), "Elin, Noah and Saga");
        assert_eq!(n(&["A", "B", "C", "D"]), "A, B and 2 more");
    }
}
