use std::rc::Rc;

use base64::Engine as _;
use mello_core::{decode_clip_waveform, Command, Event};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::app_context::AppContext;
use crate::converters::{avatar_color_index, make_initials};
use crate::head2head::{self, Person, Placement};
use crate::{
    FeedCardData, RecapAwardRow, RecapBeltHeldRow, RecapBeltMovedRow, RecapHeldTitle, RecapLbRow,
    RecentGameTile,
};

fn normalized_entry_data(raw: &serde_json::Value) -> serde_json::Value {
    if raw.is_object() {
        return raw.clone();
    }

    if let Some(encoded) = raw.as_str() {
        if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded) {
            if let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&decoded) {
                if parsed.is_object() {
                    return parsed;
                }
            }
        }
    }

    serde_json::Value::Object(serde_json::Map::new())
}

// The server feed type collapses the backend event type (session covers
// voice/game/stream, catchup covers moment/join/leave/chat). The copy helpers
// below still key off the backend type, so recover it from the payload fields.
// member_joined and member_left carry identical data, so leaves fold into the
// join copy path; everything else is distinguishable.
fn derive_backend_type(feed_type: &str, data: &serde_json::Value) -> &'static str {
    match feed_type {
        "clip" => "clip",
        "recap" => "weekly_recap",
        "session-preview" => "stream_session",
        "session" => {
            if data.get("channel_name").is_some() {
                "voice_session"
            } else if data.get("game_name").is_some() || data.get("player_names").is_some() {
                "game_session"
            } else {
                "stream_session"
            }
        }
        "catchup" => {
            if data.get("sentiment").is_some() || data.get("text").is_some() {
                "moment"
            } else if data.get("message_count").is_some() {
                "chat_activity"
            } else {
                "member_joined"
            }
        }
        "recent-games" => "recent_games",
        _ => "",
    }
}

fn extract_actor(data: &serde_json::Value, backend_type: &str) -> String {
    match backend_type {
        "stream_session" => data
            .get("streamer_name")
            .and_then(|v| v.as_str())
            .unwrap_or("someone")
            .to_string(),
        "weekly_recap" => data
            .get("most_active")
            .or_else(|| data.get("mvp"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "moment" | "member_joined" | "member_left" => data
            .get("display_name")
            .or_else(|| data.get("username"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "clip" => data
            .get("clipper_name")
            .and_then(|v| v.as_str())
            .or_else(|| {
                data.get("participant_names")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_str())
            })
            .unwrap_or("")
            .to_string(),
        _ => data
            .get("participant_names")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    }
}

fn extract_title(data: &serde_json::Value, backend_type: &str, actor: &str) -> String {
    match backend_type {
        "clip" => format!(
            "{} clipped that",
            if actor.is_empty() { "someone" } else { actor }
        ),
        "voice_session" => {
            let ch = data
                .get("channel_name")
                .and_then(|v| v.as_str())
                .unwrap_or("voice");
            let names = data
                .get("participant_names")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            if names.is_empty() {
                format!("Voice session in {}", ch)
            } else {
                format!("{} in {}", names, ch)
            }
        }
        "stream_session" => {
            let title = data
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("a stream");
            format!("{} streamed {}", actor, title)
        }
        "game_session" => {
            let game = data
                .get("game_name")
                .and_then(|v| v.as_str())
                .unwrap_or("a game");
            let names = data
                .get("player_names")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            // Telemetry record/streak (spec 18), appended when present.
            let wins = data.get("wins").and_then(|v| v.as_u64()).unwrap_or(0);
            let losses = data.get("losses").and_then(|v| v.as_u64()).unwrap_or(0);
            let streak = data
                .get("streak_after")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let record = if wins > 0 || losses > 0 {
                let streak_txt = if streak >= 2 {
                    format!(", {}-win streak", streak)
                } else if streak <= -2 {
                    format!(", {}-loss skid", -streak)
                } else {
                    String::new()
                };
                format!(" · {}W\u{2013}{}L{}", wins, losses, streak_txt)
            } else {
                String::new()
            };
            if names.is_empty() {
                format!("Played {}{}", game, record)
            } else {
                format!("{} played {}{}", names, game, record)
            }
        }
        "weekly_recap" => {
            let hangout = recap_lead_minutes(data);
            // Clips-only week: lead with the clip count, not "0m".
            let clips = data.get("clip_count").and_then(|v| v.as_i64()).unwrap_or(0);
            if hangout <= 0.0 && clips > 0 {
                return format!("{} {}", clips, if clips == 1 { "clip" } else { "clips" });
            }
            let hours = hangout / 60.0;
            if hours >= 1.0 {
                format!("{:.1}h", hours)
            } else {
                format!("{}m", hangout as i64)
            }
        }
        "moment" => {
            let text = data.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if text.is_empty() {
                let sentiment = data
                    .get("sentiment")
                    .and_then(|v| v.as_str())
                    .unwrap_or("moment");
                format!("{} had a {}", actor, sentiment)
            } else {
                text.to_string()
            }
        }
        "member_joined" => format!("{} joined the crew", actor),
        _ => String::new(),
    }
}

/// Minutes the recap card leads with: `total_min` (voice, stream and game), or
/// `total_hangout_min` for recaps stored before `total_min` existed.
fn recap_lead_minutes(data: &serde_json::Value) -> f64 {
    let num = |k: &str| data.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let total = num("total_min");
    if total > 0.0 {
        total
    } else {
        num("total_hangout_min")
    }
}

/// True for a recap entry with no minutes and no clips (sessions with zero
/// minutes do not count, matching the server `isEmptyRecap`). The server drops
/// these; the client hides any that still arrive.
fn is_empty_recap_entry(entry_type: &str, raw_data: &serde_json::Value) -> bool {
    if entry_type != "recap" {
        return false;
    }
    let data = normalized_entry_data(raw_data);
    let count = |k: &str| data.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
    recap_lead_minutes(&data) <= 0.0 && count("clip_count") <= 0.0
}

fn extract_subtitle(data: &serde_json::Value, backend_type: &str) -> String {
    match backend_type {
        "weekly_recap" => {
            // Empty when no clips, so the recap card hides the "Clips" row. Also
            // empty on a clips-only week, where the title already is the count.
            let clips = data.get("clip_count").and_then(|v| v.as_i64()).unwrap_or(0);
            if clips > 0 && recap_lead_minutes(data) > 0.0 {
                clips.to_string()
            } else {
                String::new()
            }
        }
        "clip" => {
            let clip_type = data
                .get("clip_type")
                .and_then(|v| v.as_str())
                .unwrap_or("voice");
            let game = data.get("game").and_then(|v| v.as_str()).unwrap_or("");
            if game.is_empty() {
                clip_type.to_string()
            } else {
                format!("{} · {}", clip_type, game)
            }
        }
        _ => String::new(),
    }
}

fn waveform_from_data(data: &serde_json::Value) -> ModelRc<f32> {
    let peaks = data
        .get("waveform")
        .and_then(|v| v.as_str())
        .map(decode_clip_waveform)
        .unwrap_or_default();
    ModelRc::new(slint::VecModel::from(peaks))
}

/// Which cells of the skeleton trio show (CLIPS 8). Pure, so the rule is
/// testable without a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SkeletonTrio {
    pub stream: bool,
    pub voice_clip: bool,
    pub recap: bool,
}

impl SkeletonTrio {
    pub fn any(self) -> bool {
        self.stream || self.voice_clip || self.recap
    }
}

/// Decide the trio from the card types in the feed. A cell hides when its real
/// card type exists: a stream preview or a live stream, a clip, a recap. All
/// three hide when the crew has two or more members and at least one real card
/// of any kind. `locked` is the m3llo+ upsell, not a real card.
pub(crate) fn skeleton_trio_for<'a>(
    card_types: impl IntoIterator<Item = &'a str>,
    live_streams: usize,
    member_count: i32,
) -> SkeletonTrio {
    let (mut stream, mut voice_clip, mut recap) = (live_streams == 0, true, true);
    let mut any_real = live_streams > 0;
    for t in card_types {
        // Head2head and its invite card are not real cards: frame B2 keeps the
        // trio next to a versus hero.
        if t.is_empty() || t == "locked" || t == "head2head" || t == "invite" {
            continue;
        }
        any_real = true;
        match t {
            "session-preview" => stream = false,
            "clip" => voice_clip = false,
            "recap" => recap = false,
            _ => {}
        }
    }
    if member_count >= 2 && any_real {
        return SkeletonTrio {
            stream: false,
            voice_clip: false,
            recap: false,
        };
    }
    SkeletonTrio {
        stream,
        voice_clip,
        recap,
    }
}

/// Recompute the skeleton trio and the invite line from the models on screen.
/// Call after anything that changes the feed cards, the live streams or the
/// active crew. The cells never enter the card model, so they cannot take the
/// hero slot or a grid cell.
pub(super) fn refresh_feed_skeletons(ctx: &AppContext) {
    let app = &ctx.app;
    let types: Vec<String> = [app.get_feed_cards(), app.get_memory_cards()]
        .iter()
        .flat_map(|m| (0..m.row_count()).filter_map(|i| m.row_data(i)))
        .map(|c| c.card_type.to_string())
        .collect();
    let active = app.get_active_crew_id().to_string();
    let crews = app.get_crews();
    let members = (0..crews.row_count())
        .filter_map(|i| crews.row_data(i))
        .find(|c| c.id.as_str() == active)
        .map(|c| c.member_count)
        .unwrap_or(0);
    let live = app.get_active_stream_cards().row_count();
    let trio = skeleton_trio_for(types.iter().map(String::as_str), live, members);
    let invite_hidden = ctx
        .settings
        .borrow()
        .hidden_invite_crew_ids
        .contains(&active);
    app.set_feed_skeleton_stream(trio.stream);
    app.set_feed_skeleton_voice_clip(trio.voice_clip);
    app.set_feed_skeleton_recap(trio.recap);
    // Head2head and the invite card carry their own red invite button. The
    // surface has one red button, so the line gives way.
    let has_h2h_invite = types.iter().any(|t| t == "head2head" || t == "invite");
    app.set_feed_invite_line(trio.any() && !invite_hidden && !active.is_empty() && !has_h2h_invite);
}

/// The viewer and the roster of the active crew, for naming the user ids of
/// the head2head entry.
fn h2h_roster(ctx: &AppContext) -> (Person, Vec<Person>) {
    let viewer = Person::new(
        ctx.app.get_user_id().as_str(),
        ctx.app.get_user_name().as_str(),
    );
    let model = ctx.app.get_members();
    let members: Vec<Person> = (0..model.row_count())
        .filter_map(|i| model.row_data(i))
        .map(|m| Person {
            hue: avatar_color_index(m.id.as_str()),
            id: m.id.to_string(),
            name: m.name.to_string(),
            initials: m.initials.to_string(),
        })
        .collect();
    (viewer, members)
}

/// Turn the `head2head` entry into its feed cards: the head2head card, and
/// the invite card from five members on. The placement says whether the card
/// leads the feed (wide and versus do) or keeps the server's position.
fn head2head_cards(
    ctx: &AppContext,
    e: &mello_core::crew_events::FeedEntry,
) -> Option<(Placement, Vec<FeedCardData>)> {
    let data = normalized_entry_data(&e.data);
    let (viewer, members) = h2h_roster(ctx);
    let roster = |id: &str| -> Person {
        if id == viewer.id {
            return viewer.clone();
        }
        members
            .iter()
            .find(|m| m.id == id)
            .cloned()
            .unwrap_or_else(|| Person::unknown(id))
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let model = head2head::build(&data, &viewer, &roster, now_ms)?;
    // The octagons wear the avatar the app already loaded for the user id,
    // and fall back to initials. A late avatar patches the card in place
    // (`refresh_head2head_cards`).
    let cache = ctx.avatar_cache.borrow();
    let avatar = |id: &str| cache.get(id).cloned();
    let mut cards = vec![FeedCardData {
        id: e.id.as_str().into(),
        card_type: "head2head".into(),
        role: e.role.as_str().into(),
        size: e.size.as_str().into(),
        head2head: head2head::to_slint(&model, &avatar),
        ..Default::default()
    }];
    // From five members the cell loses its seats and button. The invite card
    // takes over, with the viewer first and then the crew.
    if model.member_count >= head2head::SEATS {
        let mut shown = vec![viewer.clone()];
        shown.extend(members.iter().filter(|m| m.id != viewer.id).cloned());
        cards.push(FeedCardData {
            id: "invite".into(),
            card_type: "invite".into(),
            role: "standard".into(),
            size: "md".into(),
            head2head: head2head::invite_card_data(&shown, model.member_count, &avatar),
            ..Default::default()
        });
    }
    // A crew of five or more with nothing to compare has no cell. The invite
    // card stays.
    if model.placement == Placement::CellFull && model.pairs.is_empty() {
        cards.remove(0);
    }
    Some((model.placement, cards))
}

/// Build the this_week cards again from the last feed. The member list and the
/// feed load in no fixed order, and the head2head and invite cards name
/// members from the list: a feed that came first would show "Crew mate" and a
/// short invite card. Called when the member list is set.
pub(super) fn rebuild_roster_cards(ctx: &AppContext) {
    let Some(resp) = ctx.last_feed.borrow().clone() else {
        return;
    };
    if resp.crew_id != ctx.app.get_active_crew_id().as_str() {
        return;
    }
    let Some(section) = resp.sections.iter().find(|s| s.id == "this_week") else {
        return;
    };
    if !section.entries.iter().any(|e| e.entry_type == "head2head") {
        return;
    }
    let cards = build_this_week(ctx, &section.entries);
    ctx.app
        .set_feed_cards(Rc::new(slint::VecModel::from(cards)).into());
    refresh_feed_skeletons(ctx);
}

/// True when a head2head or invite card draws an octagon for the user. A big
/// crew loads dozens of avatars, and most belong to members the cards do not
/// draw: those need no rebuild.
fn head2head_cards_show(app: &crate::MainWindow, user_id: &str) -> bool {
    app.get_feed_cards()
        .iter()
        .any(|c| match c.card_type.as_str() {
            "head2head" => {
                let h = &c.head2head;
                h.left_id.as_str() == user_id
                    || h.right_id.as_str() == user_id
                    || h.pairs.iter().any(|r| r.id.as_str() == user_id)
            }
            "invite" => c.head2head.members.iter().any(|m| m.id.as_str() == user_id),
            _ => false,
        })
}

/// Put a newly loaded avatar on the head2head and invite cards. The two cards
/// are built again from the last feed, which reads the avatar cache, and
/// replace their rows in the feed model. Editing the avatar of a row inside
/// the card's own model changes the data but the octagons keep their
/// initials on screen, so the row is replaced instead. Without a feed, or
/// when the cards of the feed changed shape, the whole this_week is rebuilt.
pub(super) fn refresh_head2head_cards(ctx: &AppContext, user_id: &str) {
    if !head2head_cards_show(&ctx.app, user_id) {
        return;
    }
    let Some(resp) = ctx.last_feed.borrow().clone() else {
        return;
    };
    if resp.crew_id != ctx.app.get_active_crew_id().as_str() {
        return;
    }
    let Some(entry) = resp
        .sections
        .iter()
        .filter(|s| s.id == "this_week")
        .flat_map(|s| s.entries.iter())
        .find(|e| e.entry_type == "head2head")
    else {
        return;
    };
    let Some((_, cards)) = head2head_cards(ctx, entry) else {
        return;
    };
    let feed = ctx.app.get_feed_cards();
    let rows: Vec<Option<usize>> = cards
        .iter()
        .map(|c| (0..feed.row_count()).find(|&i| feed.row_data(i).is_some_and(|r| r.id == c.id)))
        .collect();
    if rows.iter().any(Option::is_none) {
        rebuild_roster_cards(ctx);
        return;
    }
    for (card, row) in cards.into_iter().zip(rows.into_iter().flatten()) {
        feed.set_row_data(row, card);
    }
}

/// The cards of the `this_week` section. Wide and versus lead the feed (the
/// wide slot and the hero slot); a cell keeps the server's position.
fn build_this_week(
    ctx: &AppContext,
    entries: &[mello_core::crew_events::FeedEntry],
) -> Vec<FeedCardData> {
    let mut lead: Vec<FeedCardData> = Vec::new();
    let mut cards: Vec<FeedCardData> = Vec::new();
    for e in entries {
        if is_empty_recap_entry(&e.entry_type, &e.data) {
            continue;
        }
        if e.entry_type == "head2head" {
            if let Some((placement, group)) = head2head_cards(ctx, e) {
                match placement {
                    Placement::Wide | Placement::Versus => lead = group,
                    Placement::Cell | Placement::CellFull => cards.extend(group),
                }
            }
            continue;
        }
        cards.push(build_feed_card(
            ctx,
            &e.id,
            &e.entry_type,
            &e.role,
            &e.size,
            &e.data,
            e.ts,
        ));
    }
    lead.extend(cards);
    lead
}

type MvpSlot = (String, String, String, String); // (name, initials, stat, user_id)

fn extract_mvps(data: &serde_json::Value, backend_type: &str) -> (i32, MvpSlot, MvpSlot, MvpSlot) {
    let empty = || {
        (
            "".to_string(),
            "".to_string(),
            "".to_string(),
            "".to_string(),
        )
    };
    if backend_type != "weekly_recap" {
        return (0, empty(), empty(), empty());
    }
    let members = match data.get("top_members").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return (0, empty(), empty(), empty()),
    };
    let to_slot = |v: &serde_json::Value| -> MvpSlot {
        let name = v
            .get("display_name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        let initials = make_initials(&name);
        let user_id = v
            .get("user_id")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        let mins = v.get("hangout_min").and_then(|n| n.as_i64()).unwrap_or(0);
        let stat = if mins >= 60 {
            format!("{:.1}h", mins as f64 / 60.0)
        } else {
            format!("{}m", mins)
        };
        (name, initials, stat, user_id)
    };
    let count = members.len().min(3) as i32;
    let s0 = members.first().map(to_slot).unwrap_or_else(empty);
    let s1 = members.get(1).map(to_slot).unwrap_or_else(empty);
    let s2 = members.get(2).map(to_slot).unwrap_or_else(empty);
    (count, s0, s1, s2)
}

/// Resolve a real avatar image from the cache by user id (spec 19). Falls back
/// to (default image, false) so callers render the initials dot instead.
fn resolve_avatar(ctx: &AppContext, user_id: &str) -> (slint::Image, bool) {
    if user_id.is_empty() {
        return (slint::Image::default(), false);
    }
    match ctx.avatar_cache.borrow().get(user_id) {
        Some(img) => (img.clone(), true),
        None => (slint::Image::default(), false),
    }
}

/// Build the recap "this week in games" leaderboard rows from `game_records`.
fn extract_recap_leaderboard(ctx: &AppContext, data: &serde_json::Value) -> Vec<RecapLbRow> {
    let arr = match data.get("game_records").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };
    arr.iter()
        .take(5)
        .enumerate()
        .map(|(i, v)| {
            let name = v
                .get("display_name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let user_id = v.get("user_id").and_then(|n| n.as_str()).unwrap_or("");
            let wins = v.get("wins").and_then(|n| n.as_i64()).unwrap_or(0);
            let losses = v.get("losses").and_then(|n| n.as_i64()).unwrap_or(0);
            let (avatar, has_avatar) = resolve_avatar(ctx, user_id);
            RecapLbRow {
                initials: make_initials(&name).into(),
                name: name.into(),
                record: format!("{}W {}L", wins, losses).into(),
                color_index: (i % 5) as i32,
                avatar,
                has_avatar,
            }
        })
        .collect()
}

/// Rows of the Belts moved section: one for each title that changed hands in
/// the recap week, from `belts_moved`. Names come from the recap, so a member
/// who left the crew still has one.
fn extract_recap_belts_moved(ctx: &AppContext, data: &serde_json::Value) -> Vec<RecapBeltMovedRow> {
    let arr = match data.get("belts_moved").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };
    let str_of = |v: &serde_json::Value, k: &str| -> String {
        v.get(k).and_then(|n| n.as_str()).unwrap_or("").to_string()
    };
    arr.iter()
        .map(|v| {
            let title_id = str_of(v, "title_id");
            let (title, icon) = head2head::belt_look(&title_id);
            let (value, unit) = head2head::value_text(
                v.get("value").unwrap_or(&serde_json::Value::Null),
                &str_of(v, "unit"),
            );
            let holder_id = str_of(v, "holder_user_id");
            let holder_name = str_of(v, "holder_name");
            let prev_id = str_of(v, "previous_holder_user_id");
            let prev_name = str_of(v, "previous_holder_name");
            let (holder_avatar, holder_has_avatar) = resolve_avatar(ctx, &holder_id);
            let (prev_avatar, prev_has_avatar) = resolve_avatar(ctx, &prev_id);
            RecapBeltMovedRow {
                title: title.into(),
                icon,
                value: value.into(),
                unit: unit.into(),
                prev_initials: if prev_name.is_empty() {
                    "".into()
                } else {
                    make_initials(&prev_name).into()
                },
                prev_name: prev_name.into(),
                prev_hue: avatar_color_index(&prev_id),
                prev_avatar,
                prev_has_avatar,
                holder_initials: make_initials(&holder_name).into(),
                holder_name: holder_name.into(),
                holder_hue: avatar_color_index(&holder_id),
                holder_avatar,
                holder_has_avatar,
            }
        })
        .collect()
}

/// Rows of the Belts held section: members in count order, from
/// `leaderboard` (the same entries as `belts_held`). A member with no titles
/// has no row.
fn extract_recap_belts_held(ctx: &AppContext, data: &serde_json::Value) -> Vec<RecapBeltHeldRow> {
    let arr = match data
        .get("leaderboard")
        .or_else(|| data.get("belts_held"))
        .and_then(|v| v.as_array())
    {
        Some(a) => a,
        None => return Vec::new(),
    };
    let mut rows: Vec<RecapBeltHeldRow> = arr
        .iter()
        .filter_map(|v| {
            let count = v.get("count").and_then(|n| n.as_i64()).unwrap_or(0);
            if count <= 0 {
                return None;
            }
            let user_id = v.get("user_id").and_then(|n| n.as_str()).unwrap_or("");
            let name = v
                .get("display_name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let (avatar, has_avatar) = resolve_avatar(ctx, user_id);
            let titles: Vec<RecapHeldTitle> = v
                .get("title_ids")
                .and_then(|t| t.as_array())
                .map(|ids| {
                    ids.iter()
                        .filter_map(|i| i.as_str())
                        .map(|id| {
                            let (title, icon) = head2head::belt_look(id);
                            RecapHeldTitle {
                                title: title.into(),
                                icon,
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some((
                count,
                RecapBeltHeldRow {
                    initials: make_initials(&name).into(),
                    name: name.into(),
                    hue: avatar_color_index(user_id),
                    avatar,
                    has_avatar,
                    count: count as i32,
                    titles: ModelRc::new(VecModel::from(titles)),
                },
            ))
        })
        .map(|(_, r)| r)
        .collect();
    // The backend sends them most first. Keep that order for equal counts.
    rows.sort_by_key(|r| std::cmp::Reverse(r.count));
    rows
}

fn extract_recap_awards(data: &serde_json::Value) -> Vec<RecapAwardRow> {
    let arr = match data.get("awards").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };
    arr.iter()
        // Belt awards are drawn as the Belts moved section, with the two members.
        .filter(|v| v.get("kind").and_then(|k| k.as_str()) != Some("belt"))
        .map(|v| {
            let s = |k: &str| {
                v.get(k)
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string()
                    .into()
            };
            RecapAwardRow {
                kind: s("kind"),
                title: s("title"),
                name: s("name"),
                detail: s("detail"),
            }
        })
        .collect()
}

/// Compact "this week in games" summary, e.g. "312 matches · CS2, Rocket League".
fn games_summary(data: &serde_json::Value) -> String {
    let arr = match data.get("games_played").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return String::new(),
    };
    let total: i64 = arr
        .iter()
        .filter_map(|v| v.get("matches").and_then(|m| m.as_i64()))
        .sum();
    if total == 0 {
        return String::new();
    }
    let games: Vec<String> = arr
        .iter()
        .take(3)
        .filter_map(|v| v.get("game").and_then(|g| g.as_str()).map(String::from))
        .collect();
    if games.is_empty() {
        format!("{} matches", total)
    } else {
        format!("{} matches · {}", total, games.join(", "))
    }
}

/// Humanized session length for the redesigned cards: "3m", "47m", "9h 7m".
/// Sub-minute sessions round up to "1m" — "0:45" style strings read as
/// ambiguous clock times (the exact complaint about the old card).
fn humanize_duration(duration_secs: f64) -> String {
    if duration_secs <= 0.0 {
        return String::new();
    }
    let mins = ((duration_secs / 60.0).ceil() as u64).max(1);
    humanize_duration_min(mins as i64)
}

fn humanize_duration_min(mins: i64) -> String {
    if mins <= 0 {
        return String::new();
    }
    let mins = mins.max(1);
    if mins >= 60 {
        format!("{}h {}m", mins / 60, mins % 60)
    } else {
        format!("{}m", mins)
    }
}

/// Header line of the recent games card: "14 sessions, 31 h this week".
/// Under an hour the total reads in minutes. One session reads "1 session".
/// No sessions (a card built from the installed library only) gives no line.
fn recent_games_header(session_count: i64, total_min: i64) -> String {
    if session_count <= 0 {
        return String::new();
    }
    let sessions = if session_count == 1 {
        "1 session".to_string()
    } else {
        format!("{session_count} sessions")
    };
    let total = if total_min < 60 {
        format!("{} min", total_min.max(1))
    } else {
        // Round to the nearest hour: the line is a gist, the tiles carry detail.
        format!("{} h", (total_min + 30) / 60)
    };
    format!("{sessions}, {total} this week")
}

/// Tiles for the recent games card, keyed by game id, from the server's
/// `games` array, and whether every tile is installed on this device.
///
/// Art ladder: the runtime icon (cached or crew-shared), else the Steam header
/// art on this disk, else the monogram. Nothing leaves the device. The log
/// carries counts only.
fn extract_recent_game_tiles(
    ctx: &AppContext,
    data: &serde_json::Value,
) -> (Vec<RecentGameTile>, bool) {
    let installed = ctx.installed_games.borrow();
    let installed_ids: std::collections::HashSet<String> =
        installed.iter().map(|g| g.game_id.clone()).collect();
    let installed_names: std::collections::HashSet<String> = installed
        .iter()
        .map(|g| g.name.trim().to_lowercase())
        .collect();
    drop(installed);

    let rows = data
        .get("games")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let (mut local_art, mut tracked_count, mut installed_count) = (0, 0, 0);
    let tiles: Vec<RecentGameTile> = rows
        .iter()
        .filter_map(|row| {
            let game_name = row.get("game_name")?.as_str()?;
            let wire_id = row.get("game_id").and_then(|v| v.as_str()).unwrap_or("");
            let minutes = row.get("minutes").and_then(|v| v.as_i64()).unwrap_or(0);
            let wire_launcher = row.get("launcher").and_then(|v| v.as_str()).unwrap_or("");
            let (game_id, _, _) = resolve_game_meta(ctx, wire_id, game_name);

            let (game_icon, has_icon) =
                match crate::game_icons::resolve_or_fetch_icon(ctx, &game_id) {
                    Some(img) => (img, true),
                    None => (slint::Image::default(), false),
                };
            // The local header is tried under the id the server sent: a
            // launcher id (`steam-730`) is what names the Steam app.
            let (capsule, has_capsule) = if has_icon {
                (slint::Image::default(), false)
            } else {
                [wire_id, game_id.as_str()]
                    .iter()
                    .find_map(|id| crate::game_icons::resolve_local_steam_art(ctx, id))
                    .map_or((slint::Image::default(), false), |img| (img, true))
            };
            if has_capsule {
                local_art += 1;
            }
            let tracked = mello_core::game_sensing::is_tracked_game(wire_id, game_name)
                || mello_core::game_sensing::is_tracked_game(&game_id, game_name);
            if tracked {
                tracked_count += 1;
            }
            if crate::recent_games::is_installed(
                wire_id,
                game_name,
                &installed_ids,
                &installed_names,
            ) || crate::recent_games::is_installed(
                &game_id,
                "",
                &installed_ids,
                &installed_names,
            ) {
                installed_count += 1;
            }
            let launcher = crate::recent_games::launcher_of(wire_id, wire_launcher);
            Some(RecentGameTile {
                game_id: game_id.as_str().into(),
                game_name: game_name.into(),
                monogram: crate::recent_games::monogram(game_name).into(),
                hue_index: crate::recent_games::hue_index(&game_id, game_name),
                game_icon,
                has_icon,
                capsule,
                has_capsule,
                hours: crate::recent_games::hours_label(minutes, &launcher).into(),
                tracked,
            })
        })
        .collect();
    let all_installed = !tiles.is_empty() && installed_count == tiles.len();
    log::info!(
        "[recent-games] {} tile(s), {} with local art, {} tracked, {} installed",
        tiles.len(),
        local_art,
        tracked_count,
        installed_count
    );
    (tiles, all_installed)
}

/// Wall-time duration for game sessions; uses active time when wall time is
/// inflated (left running overnight). GAME-SURFACING §4.
fn game_session_duration_display(duration_min: i64, active_min: i64) -> String {
    let use_active = active_min > 0 && active_min * 3 < duration_min;
    let display_min = if use_active { active_min } else { duration_min };
    let body = humanize_duration_min(display_min);
    if body.is_empty() {
        return body;
    }
    if use_active {
        format!("~{body}")
    } else {
        body
    }
}

/// Resolve `(game_id, short_name, color_hex)` for a card: the bundled DB by
/// id, then by display name (stream sessions and legacy events carry only a
/// name), then the user's confirmed custom games. Unresolved names keep an
/// empty id/short-name so the badge falls back to the raw name.
fn resolve_game_meta(
    _ctx: &AppContext,
    game_id: &str,
    game_name: &str,
) -> (String, String, Option<String>) {
    thread_local! {
        static CATALOGUE: std::cell::OnceCell<Option<mello_core::catalogue::Head>> =
            const { std::cell::OnceCell::new() };
    }
    let catalogued = CATALOGUE.with(|head| {
        let head = head.get_or_init(mello_core::catalogue::Head::bundled);
        let head = head.as_ref()?;
        let entry = if !game_id.is_empty() {
            head.by_game_id(game_id)
        } else {
            None
        }
        .or_else(|| {
            // Stream sessions and pre-`game_id` events carry only a name.
            (!game_name.is_empty())
                .then(|| head.by_name(game_name))
                .flatten()
        })?;
        Some((
            entry.game_id.to_string(),
            entry.short_name.to_string(),
            None,
        ))
    });
    if let Some(resolved) = catalogued {
        return resolved;
    }

    // Everything the catalogue has never heard of — a game discovered in an
    // installed library, or one nothing could name — still needs a badge.
    // Deriving from the display name is what stops those cards rendering a
    // blank square, which is what happened while this fell back to "".
    let short = if game_name.is_empty() {
        String::new()
    } else {
        mello_core::library::derive_short_name(game_name)
    };
    (game_id.to_string(), short, None)
}

// Build a feed card from a server feed entry. card_type is the server-provided
// feed type; the backend type is recovered for copy extraction.
fn build_feed_card(
    ctx: &AppContext,
    id: &str,
    feed_type: &str,
    role: &str,
    size: &str,
    raw_data: &serde_json::Value,
    ts: i64,
) -> FeedCardData {
    let data = normalized_entry_data(raw_data);
    let backend_type = derive_backend_type(feed_type, &data);

    let snapshot_urls: Vec<String> = data
        .get("snapshot_urls")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let has_snapshots = !snapshot_urls.is_empty();

    let actor = extract_actor(&data, backend_type);
    let title = extract_title(&data, backend_type, &actor);
    let subtitle = extract_subtitle(&data, backend_type);

    let duration_secs = data
        .get("duration_seconds")
        .and_then(|v| v.as_f64())
        .or_else(|| {
            data.get("duration_min")
                .and_then(|v| v.as_f64())
                .map(|m| m * 60.0)
        })
        .or_else(|| {
            data.get("longest_session_min")
                .and_then(|v| v.as_f64())
                .map(|m| m * 60.0)
        })
        .unwrap_or(0.0);

    let duration_min = data
        .get("duration_min")
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;

    let duration_str = if duration_secs >= 3600.0 {
        let h = (duration_secs / 3600.0) as u32;
        let m = ((duration_secs % 3600.0) / 60.0) as u32;
        format!("{}h {}m", h, m)
    } else if duration_secs > 0.0 {
        let secs = duration_secs as u32;
        format!("{}:{:02}", secs / 60, secs % 60)
    } else {
        String::new()
    };

    let game = data
        .get("game")
        .or_else(|| data.get("game_name"))
        .or_else(|| data.get("top_game"))
        .or_else(|| data.get("title"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Stable id for the game icon; empty on legacy events and stream
    // sessions, where it is recovered from the game name below.
    let game_id = data
        .get("game_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let (game_id, game_short_name, game_color_hex) = resolve_game_meta(ctx, &game_id, &game);
    let game_has_color = game_color_hex.is_some();
    let game_color = slint::Color::from_argb_encoded(super::game::parse_hex_color(
        game_color_hex.as_deref().unwrap_or("#2a2a30"),
    ));
    // Runtime icon (exe-extracted / crew-shared) for games without bundled
    // art; a cache miss requests the crew copy once per run.
    let (game_icon, game_has_icon) = match crate::game_icons::resolve_or_fetch_icon(ctx, &game_id) {
        Some(img) => (img, true),
        None => (slint::Image::default(), false),
    };

    let clip_path = data
        .get("media_url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| data.get("local_path").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();

    let participant_count = data
        .get("participants")
        .or_else(|| data.get("participant_ids"))
        .or_else(|| data.get("player_ids"))
        .and_then(|v| v.as_array())
        .map(|a| a.len() as i32)
        .or_else(|| {
            data.get("peak_count")
                .or_else(|| data.get("peak_viewers"))
                .and_then(|v| v.as_i64())
                .map(|n| n as i32)
        })
        .unwrap_or(0);

    let clip_count = data
        .get("clip_count")
        .and_then(|v| v.as_i64())
        .map(|n| n.max(0) as i32)
        .unwrap_or(0);

    let ts_secs = ts / 1000;
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let ago = now_secs - ts_secs;
    let timestamp = if ago < 60 {
        "just now".to_string()
    } else if ago < 3600 {
        format!("{}m ago", ago / 60)
    } else if ago < 43200 {
        format!("{}h ago", ago / 3600)
    } else if ago < 86400 {
        "yesterday".to_string()
    } else if ago < 172800 {
        "2 days ago".to_string()
    } else if ago < 604800 {
        let days = ago / 86400;
        let weekday = match days % 7 {
            0 => "today",
            1 => "yesterday",
            _ => match (ts_secs / 86400 + 4) % 7 {
                0 => "Sunday",
                1 => "Monday",
                2 => "Tuesday",
                3 => "Wednesday",
                4 => "Thursday",
                5 => "Friday",
                _ => "Saturday",
            },
        };
        weekday.to_string()
    } else {
        format!("{}d ago", ago / 86400)
    };

    // Rich game-session card fields (spec 19 B2). Retype game sessions so the
    // feed dispatches them to the rich GameSessionCard instead of SessionCard.
    let is_game_session = backend_type == "game_session";
    let is_recent_games = feed_type == "recent-games";
    let card_type = if is_game_session {
        "game-session"
    } else {
        feed_type
    };
    let (recent_games, recent_all_installed) = if is_recent_games {
        extract_recent_game_tiles(ctx, &data)
    } else {
        (Vec::new(), false)
    };
    let (title, subtitle) = if is_recent_games {
        let session_count = data
            .get("session_count")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let total_min = data.get("total_min").and_then(|v| v.as_i64()).unwrap_or(0);
        (
            "RECENT GAMES".to_string(),
            crate::recent_games::header_note(
                recent_all_installed,
                recent_games_header(session_count, total_min),
            ),
        )
    } else {
        (title, subtitle)
    };
    let game_verified = is_game_session
        && data
            .get("verified")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    let (
        game_record,
        game_wins,
        game_losses,
        game_draws,
        game_streak_text,
        game_streak_positive,
        game_winrate,
        game_matches,
    ) = if is_game_session {
        let w = data.get("wins").and_then(|v| v.as_i64()).unwrap_or(0);
        let l = data.get("losses").and_then(|v| v.as_i64()).unwrap_or(0);
        let d = data.get("draws").and_then(|v| v.as_i64()).unwrap_or(0);
        let streak = data
            .get("streak_after")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let matches = w + l + d;
        let record = if matches > 0 {
            if d > 0 {
                format!("{}W\u{2013}{}L\u{2013}{}D", w, l, d)
            } else {
                format!("{}W\u{2013}{}L", w, l)
            }
        } else {
            String::new()
        };
        let streak_text = if streak > 0 {
            format!("W{streak}")
        } else if streak < 0 {
            format!("L{}", -streak)
        } else {
            "—".to_string()
        };
        let decided = w + l;
        let winrate = if decided > 0 {
            format!("{}%", (w * 100) / decided)
        } else {
            "—".to_string()
        };
        (
            record,
            w as i32,
            l as i32,
            d as i32,
            streak_text,
            streak >= 0,
            winrate,
            matches as i32,
        )
    } else {
        (
            String::new(),
            0,
            0,
            0,
            String::new(),
            true,
            String::new(),
            0,
        )
    };

    // --- Session card redesign (voice/stream): typed, person-aware fields ---
    let session_kind = if feed_type == "session" || feed_type == "session-preview" {
        match backend_type {
            "stream_session" => "stream",
            "voice_session" => "voice",
            _ => "",
        }
    } else {
        ""
    };
    let duration_human = if is_game_session {
        let active_min = data.get("active_min").and_then(|v| v.as_i64()).unwrap_or(0);
        game_session_duration_display(duration_min as i64, active_min)
    } else {
        humanize_duration(duration_secs)
    };
    let peak_count = data
        .get("peak_viewers")
        .or_else(|| data.get("peak_count"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    let channel_name = data
        .get("channel_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // Actor avatar: the streamer for streams, the session owner for game
    // sessions (badged on the icon / leading the footer stack).
    let actor_avatar_id = match backend_type {
        "stream_session" => data.get("streamer_id").and_then(|v| v.as_str()),
        "game_session" => data
            .get("player_ids")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str()),
        _ => None,
    };
    let (actor_avatar, actor_has_avatar) = match actor_avatar_id {
        Some(id) if !id.is_empty() => resolve_avatar(ctx, id),
        _ => (slint::Image::default(), false),
    };
    // Voice sessions carry participant_*; game sessions carry player_*.
    let participant_names: Vec<String> = data
        .get("participant_names")
        .or_else(|| data.get("player_names"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let participants_line = match participant_names.len() {
        0 => String::new(),
        1 | 2 => participant_names.join(", "),
        n => format!(
            "{} +{}",
            participant_names[..2].join(", "),
            n.saturating_sub(2)
        ),
    };
    let participant_ids: Vec<String> = data
        .get("participant_ids")
        .or_else(|| data.get("player_ids"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let part_slot = |i: usize| -> (slint::SharedString, slint::Image, bool) {
        let initials = participant_names
            .get(i)
            .map(|n| make_initials(n))
            .unwrap_or_default();
        let (avatar, has) = participant_ids
            .get(i)
            .map(|id| resolve_avatar(ctx, id))
            .unwrap_or((slint::Image::default(), false));
        (initials.into(), avatar, has)
    };
    let (part0_initials, part0_avatar, part0_has_avatar) = part_slot(0);
    let (part1_initials, part1_avatar, part1_has_avatar) = part_slot(1);
    let (part2_initials, part2_avatar, part2_has_avatar) = part_slot(2);

    let (mvp_count, mvp0, mvp1, mvp2) = extract_mvps(&data, backend_type);
    let (mvp0_av, mvp0_has_av) = resolve_avatar(ctx, &mvp0.3);
    let (mvp1_av, mvp1_has_av) = resolve_avatar(ctx, &mvp1.3);
    let (mvp2_av, mvp2_has_av) = resolve_avatar(ctx, &mvp2.3);
    let recap_leaderboard = extract_recap_leaderboard(ctx, &data);
    let recap_awards = extract_recap_awards(&data);
    let recap_belts_moved = extract_recap_belts_moved(ctx, &data);
    let recap_belts_held = extract_recap_belts_held(ctx, &data);
    let recap_games_summary = games_summary(&data);
    let was_seen = ctx
        .settings
        .borrow()
        .seen_session_ids
        .iter()
        .any(|s| s == id);

    let duration_ms = (duration_secs * 1000.0).round() as i32;
    let waveform = if backend_type == "clip" {
        waveform_from_data(&data)
    } else {
        ModelRc::new(slint::VecModel::from(Vec::<f32>::new()))
    };

    FeedCardData {
        id: id.into(),
        card_type: card_type.into(),
        role: role.into(),
        size: size.into(),
        title: title.into(),
        subtitle: subtitle.into(),
        timestamp: timestamp.into(),
        duration: duration_str.into(),
        duration_min,
        actor_name: actor.clone().into(),
        actor_initials: make_initials(&actor).into(),
        game_name: game.into(),
        game_id: game_id.into(),
        game_short_name: game_short_name.into(),
        game_color,
        game_has_color,
        game_icon,
        game_has_icon,
        session_kind: session_kind.into(),
        duration_human: duration_human.into(),
        peak_count,
        channel_name: channel_name.into(),
        participants_line: participants_line.into(),
        actor_avatar,
        actor_has_avatar,
        part0_initials,
        part0_avatar,
        part0_has_avatar,
        part1_initials,
        part1_avatar,
        part1_has_avatar,
        part2_initials,
        part2_avatar,
        part2_has_avatar,
        participant_count,
        clip_count,
        clip_path: clip_path.into(),
        waveform,
        duration_ms,
        is_hero: role == "hero",
        is_skeleton: false,
        snapshot_urls: Rc::new(slint::VecModel::from(
            snapshot_urls
                .into_iter()
                .map(slint::SharedString::from)
                .collect::<Vec<_>>(),
        ))
        .into(),
        is_new: false,
        was_seen,
        mvp_count,
        mvp0_name: mvp0.0.into(),
        mvp0_initials: mvp0.1.into(),
        mvp0_stat: mvp0.2.into(),
        mvp1_name: mvp1.0.into(),
        mvp1_initials: mvp1.1.into(),
        mvp1_stat: mvp1.2.into(),
        mvp2_name: mvp2.0.into(),
        mvp2_initials: mvp2.1.into(),
        mvp2_stat: mvp2.2.into(),
        mvp0_avatar: mvp0_av,
        mvp0_has_avatar: mvp0_has_av,
        mvp1_avatar: mvp1_av,
        mvp1_has_avatar: mvp1_has_av,
        mvp2_avatar: mvp2_av,
        mvp2_has_avatar: mvp2_has_av,
        games_summary: recap_games_summary.into(),
        leaderboard: ModelRc::new(VecModel::from(recap_leaderboard)),
        awards: ModelRc::new(VecModel::from(recap_awards)),
        belts_moved: ModelRc::new(VecModel::from(recap_belts_moved)),
        belts_held: ModelRc::new(VecModel::from(recap_belts_held)),
        game_record: game_record.into(),
        game_wins,
        game_losses,
        game_draws,
        game_streak_text: game_streak_text.into(),
        game_streak_positive,
        game_winrate: game_winrate.into(),
        game_matches,
        game_verified,
        recent_games: ModelRc::new(VecModel::from(recent_games)),
        snapshot_loading: feed_type == "session-preview" && has_snapshots,
        snapshot_poster_ready: false,
        snapshot_error: false,
        snapshot_playback_index: 0,
        snapshot_playback_revision: 0,
        ..Default::default()
    }
}

pub fn handle(ctx: &AppContext, event: Event) {
    match event {
        Event::ClipBufferStarted => {
            log::info!("clip buffer started");
        }
        Event::ClipBufferStopped => {
            log::info!("clip buffer stopped");
        }
        Event::ClipCaptured {
            clip_id,
            path,
            duration_seconds,
            waveform,
        } => {
            log::info!(
                "clip captured: id={} path={} duration={:.1}s",
                clip_id,
                path,
                duration_seconds
            );

            // Insert an optimistic "NEW" clip card into the feed immediately
            let secs = duration_seconds as u32;
            let dur_str = format!("{}:{:02}", secs / 60, secs % 60);
            let waveform_model: ModelRc<f32> =
                ModelRc::new(slint::VecModel::from(decode_clip_waveform(&waveform)));
            let new_card = FeedCardData {
                id: clip_id.clone().into(),
                card_type: "clip".into(),
                role: "standard".into(),
                size: "md".into(),
                title: "".into(),
                subtitle: "".into(),
                timestamp: "just now".into(),
                duration: dur_str.into(),
                duration_ms: (duration_seconds * 1000.0).round() as i32,
                actor_name: "You".into(),
                actor_initials: "Y".into(),
                game_name: "".into(),
                participant_count: 1,
                clip_count: 0,
                clip_path: path.clone().into(),
                waveform: waveform_model,
                is_hero: false,
                is_skeleton: false,
                is_new: true,
                ..Default::default()
            };

            let existing = ctx.app.get_feed_cards();
            let mut cards: Vec<FeedCardData> = (0..existing.row_count())
                .filter_map(|i| existing.row_data(i))
                .collect();

            // Insert after hero (index 0) if there's a hero, otherwise at start
            let insert_pos = if !cards.is_empty() && cards[0].is_hero {
                // After hero but before recap (recap at index 1)
                if cards.len() > 1 && cards[1].card_type == "recap" {
                    2
                } else {
                    1
                }
            } else {
                0
            };
            cards.insert(insert_pos.min(cards.len()), new_card);
            ctx.app
                .set_feed_cards(Rc::new(slint::VecModel::from(cards)).into());
            refresh_feed_skeletons(ctx);

            let crew_id = ctx.app.get_active_crew_id().to_string();
            if !crew_id.is_empty() {
                let _ = ctx.cmd_tx.send(Command::PostClip {
                    crew_id: crew_id.clone(),
                    clip_id: clip_id.clone(),
                    duration_seconds: duration_seconds as f64,
                    local_path: path.clone(),
                    waveform: waveform.clone(),
                });
                let _ = ctx.cmd_tx.send(Command::UploadClip {
                    crew_id,
                    clip_id,
                    wav_path: path,
                });
            }
        }
        Event::ClipCaptureFailed { reason } => {
            log::warn!("clip capture failed: {}", reason);
        }
        Event::ClipPosted { clip_id, event_id } => {
            log::info!("clip posted: clip_id={} event_id={}", clip_id, event_id);

            let crew_id = ctx.app.get_active_crew_id().to_string();
            if !crew_id.is_empty() {
                let _ = ctx.cmd_tx.send(Command::LoadCrewFeed { crew_id });
            }
        }
        Event::ClipUploaded { clip_id, media_url } => {
            log::info!("clip uploaded: clip_id={} media_url={}", clip_id, media_url);
        }
        Event::FeedLoaded { response } => {
            log::info!(
                "feed loaded for crew {}: {} sections",
                response.crew_id,
                response.sections.len()
            );

            // The server curates this_week (order, hero, sizing); the client
            // renders it as-is. memory is the durable spine shown below.
            let mut this_week: Vec<FeedCardData> = Vec::new();
            let mut memory: Vec<FeedCardData> = Vec::new();
            for section in &response.sections {
                match section.id.as_str() {
                    "this_week" => {
                        this_week = build_this_week(ctx, &section.entries);
                    }
                    "memory" => {
                        memory = section
                            .entries
                            .iter()
                            .filter(|e| !is_empty_recap_entry(&e.entry_type, &e.data))
                            .map(|e| {
                                build_feed_card(
                                    ctx,
                                    &e.id,
                                    &e.entry_type,
                                    &e.role,
                                    &e.size,
                                    &e.data,
                                    e.ts,
                                )
                            })
                            .collect();
                    }
                    _ => {}
                }
            }

            *ctx.last_feed.borrow_mut() = Some(response.clone());
            let clip_count = this_week.iter().filter(|c| c.card_type == "clip").count() as i32;

            // The feed shows real cards only. The skeleton trio is separate
            // state, set below, so it never takes a card slot or the hero.
            let has_real_memory = memory.iter().any(|c| c.card_type != "locked");
            // Head2head and the invite card are there from day one, so they do
            // not end the cold start.
            let is_cold = this_week
                .iter()
                .all(|c| matches!(c.card_type.as_str(), "head2head" | "invite"))
                && !has_real_memory;
            let cards = this_week;

            ctx.app
                .set_feed_cards(Rc::new(slint::VecModel::from(cards)).into());
            ctx.app
                .set_memory_cards(Rc::new(slint::VecModel::from(memory)).into());
            ctx.app.set_feed_cold_start(is_cold);
            refresh_feed_skeletons(ctx);
            ctx.app.set_feed_clip_count(clip_count);
            ctx.app.set_feed_has_more(false);

            let gen = ctx.snapshot_loader.bump_generation();
            ctx.snapshot_loader
                .load_session_preview_cards(ctx.app.as_weak(), gen);

            // Update clip-count on the active crew's sidebar card
            let active_crew_id = ctx.app.get_active_crew_id().to_string();
            if !active_crew_id.is_empty() {
                let crews = ctx.app.get_crews();
                for i in 0..crews.row_count() {
                    let mut crew = crews.row_data(i).unwrap();
                    if crew.id == active_crew_id.as_str() {
                        crew.clip_count = clip_count;
                        crews.set_row_data(i, crew);
                        break;
                    }
                }
            }
        }
        Event::ClipPlaybackStarted {
            clip_path,
            duration_ms,
        } => {
            log::info!(
                "clip playback started: path={} duration={}ms",
                clip_path,
                duration_ms
            );
            ctx.app.set_clip_playing_path(clip_path.as_str().into());
            ctx.app.set_clip_progress(0.0);
            ctx.app.set_clip_paused(false);
            ctx.app.set_clip_anim_tick(0.0);
            ctx.app.set_clip_position_text("0:00".into());
            let dur_text = format!("{}:{:02}", duration_ms / 60000, (duration_ms / 1000) % 60);
            ctx.app.set_clip_duration_text(dur_text.as_str().into());
            ctx.app.set_clip_duration_ms(duration_ms as i32);
        }
        Event::ClipPlaybackProgress {
            position_ms,
            duration_ms,
        } => {
            let progress = if duration_ms > 0 {
                position_ms as f32 / duration_ms as f32
            } else {
                0.0
            };
            ctx.app.set_clip_progress(progress);
            let pos_text = format!("{}:{:02}", position_ms / 60000, (position_ms / 1000) % 60);
            ctx.app.set_clip_position_text(pos_text.as_str().into());

            // Drive animation tick (increment by ~0.15 per ~60ms progress event)
            let current = ctx.app.get_clip_anim_tick();
            ctx.app.set_clip_anim_tick(current + 0.15);
        }
        Event::ClipPlaybackFinished => {
            log::info!("clip playback finished");
            ctx.app.set_clip_playing_path("".into());
            ctx.app.set_clip_progress(0.0);
            ctx.app.set_clip_paused(false);
            ctx.app.set_clip_anim_tick(0.0);
            ctx.app.set_clip_position_text("".into());
            ctx.app.set_clip_duration_text("".into());
            ctx.app.set_clip_duration_ms(0);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_trio_rule() {
        let all = |t| (t, t, t);
        let s = |t: SkeletonTrio| (t.stream, t.voice_clip, t.recap);
        assert_eq!(s(skeleton_trio_for([], 0, 1)), all(true));
        assert_eq!(
            s(skeleton_trio_for(["session-preview"], 0, 1)),
            (false, true, true)
        );
        assert_eq!(s(skeleton_trio_for(["clip"], 0, 1)), (true, false, true));
        assert_eq!(s(skeleton_trio_for(["recap"], 0, 1)), (true, true, false));
        assert_eq!(s(skeleton_trio_for([], 1, 1)), (false, true, true));
        assert_eq!(s(skeleton_trio_for(["locked"], 0, 5)), all(true));
        assert_eq!(s(skeleton_trio_for(["catchup"], 0, 2)), all(false));
        assert_eq!(s(skeleton_trio_for([], 0, 9)), all(true));
    }

    #[test]
    fn recent_games_header_wording() {
        assert_eq!(recent_games_header(1, 45), "1 session, 45 min this week");
        assert_eq!(recent_games_header(2, 240), "2 sessions, 4 h this week");
        assert_eq!(recent_games_header(14, 1860), "14 sessions, 31 h this week");
        assert_eq!(recent_games_header(0, 0), "", "installed games only");
    }

    #[test]
    fn game_session_duration_uses_wall_time_by_default() {
        assert_eq!(game_session_duration_display(252, 0), "4h 12m");
        assert_eq!(game_session_duration_display(45, 0), "45m");
    }

    #[test]
    fn game_session_duration_uses_active_when_wall_inflated() {
        assert_eq!(game_session_duration_display(252, 65), "~1h 5m");
    }

    #[test]
    fn game_session_duration_keeps_wall_when_active_not_under_third() {
        // active * 3 must be strictly less than wall time.
        assert_eq!(game_session_duration_display(60, 20), "1h 0m");
    }
}
