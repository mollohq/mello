use std::rc::Rc;

use base64::Engine as _;
use mello_core::{Command, Event};
use slint::{ComponentHandle, Model};

use crate::app_context::AppContext;
use crate::converters::make_initials;
use crate::FeedCardData;

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
            if names.is_empty() {
                format!("Played {}", game)
            } else {
                format!("{} played {}", names, game)
            }
        }
        "weekly_recap" => {
            let hangout = data
                .get("total_hangout_min")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
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

fn extract_subtitle(data: &serde_json::Value, backend_type: &str) -> String {
    match backend_type {
        "weekly_recap" => {
            let clips = data.get("clip_count").and_then(|v| v.as_i64()).unwrap_or(0);
            format!("{}", clips)
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

fn skeleton_card(card_type: &str) -> FeedCardData {
    FeedCardData {
        id: Default::default(),
        card_type: card_type.into(),
        title: Default::default(),
        subtitle: Default::default(),
        timestamp: Default::default(),
        duration: Default::default(),
        duration_min: 0,
        actor_name: Default::default(),
        actor_initials: Default::default(),
        game_name: Default::default(),
        participant_count: 0,
        clip_count: 0,
        clip_path: Default::default(),
        is_hero: false,
        is_skeleton: true,
        snapshot_urls: Default::default(),
        mvp_count: 0,
        mvp0_name: Default::default(),
        mvp0_initials: Default::default(),
        mvp0_stat: Default::default(),
        mvp1_name: Default::default(),
        mvp1_initials: Default::default(),
        mvp1_stat: Default::default(),
        mvp2_name: Default::default(),
        mvp2_initials: Default::default(),
        mvp2_stat: Default::default(),
        is_new: false,
        was_seen: false,
        ..Default::default()
    }
}

type MvpSlot = (String, String, String); // (name, initials, stat)

fn extract_mvps(data: &serde_json::Value, backend_type: &str) -> (i32, MvpSlot, MvpSlot, MvpSlot) {
    let empty = || ("".to_string(), "".to_string(), "".to_string());
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
        let mins = v.get("hangout_min").and_then(|n| n.as_i64()).unwrap_or(0);
        let stat = if mins >= 60 {
            format!("{:.1}h", mins as f64 / 60.0)
        } else {
            format!("{}m", mins)
        };
        (name, initials, stat)
    };
    let count = members.len().min(3) as i32;
    let s0 = members.first().map(to_slot).unwrap_or_else(empty);
    let s1 = members.get(1).map(to_slot).unwrap_or_else(empty);
    let s2 = members.get(2).map(to_slot).unwrap_or_else(empty);
    (count, s0, s1, s2)
}

// Build a feed card from a server feed entry. card_type is the server-provided
// feed type; the backend type is recovered for copy extraction.
fn build_feed_card(
    ctx: &AppContext,
    id: &str,
    feed_type: &str,
    raw_data: &serde_json::Value,
    ts: i64,
    is_hero: bool,
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

    let (mvp_count, mvp0, mvp1, mvp2) = extract_mvps(&data, backend_type);
    let was_seen = ctx
        .settings
        .borrow()
        .seen_session_ids
        .iter()
        .any(|s| s == id);

    FeedCardData {
        id: id.into(),
        card_type: feed_type.into(),
        title: title.into(),
        subtitle: subtitle.into(),
        timestamp: timestamp.into(),
        duration: duration_str.into(),
        duration_min,
        actor_name: actor.clone().into(),
        actor_initials: make_initials(&actor).into(),
        game_name: game.into(),
        participant_count,
        clip_count,
        clip_path: clip_path.into(),
        is_hero,
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
            let new_card = FeedCardData {
                id: clip_id.clone().into(),
                card_type: "clip".into(),
                title: "".into(),
                subtitle: "".into(),
                timestamp: "just now".into(),
                duration: dur_str.into(),
                actor_name: "You".into(),
                actor_initials: "Y".into(),
                game_name: "".into(),
                participant_count: 1,
                clip_count: 0,
                clip_path: path.clone().into(),
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

            let crew_id = ctx.app.get_active_crew_id().to_string();
            if !crew_id.is_empty() {
                let _ = ctx.cmd_tx.send(Command::PostClip {
                    crew_id: crew_id.clone(),
                    clip_id: clip_id.clone(),
                    duration_seconds: duration_seconds as f64,
                    local_path: path.clone(),
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
                        this_week = section
                            .entries
                            .iter()
                            .map(|e| {
                                build_feed_card(
                                    ctx,
                                    &e.id,
                                    &e.entry_type,
                                    &e.data,
                                    e.ts,
                                    e.role == "hero",
                                )
                            })
                            .collect();
                    }
                    "memory" => {
                        memory = section
                            .entries
                            .iter()
                            .map(|e| {
                                build_feed_card(ctx, &e.id, &e.entry_type, &e.data, e.ts, false)
                            })
                            .collect();
                    }
                    _ => {}
                }
            }

            let clip_count = this_week.iter().filter(|c| c.card_type == "clip").count() as i32;

            // Fill remaining this_week grid slots with skeletons for cold / semi-cold start.
            let mut ordered = this_week;
            let has_hero = ordered.first().map(|c| c.is_hero).unwrap_or(false);
            let has_recap = ordered.iter().any(|c| c.card_type == "recap");
            let has_session = ordered.iter().any(|c| c.card_type == "session");

            if !has_hero {
                ordered.insert(0, skeleton_card("skeleton-hero"));
            }
            if !has_recap {
                let pos = 1.min(ordered.len());
                ordered.insert(pos, skeleton_card("skeleton-recap"));
            }

            let mut fillers: Vec<&str> = Vec::new();
            if !has_session {
                fillers.push("skeleton-session");
            }
            fillers.extend_from_slice(&[
                "skeleton-clip",
                "skeleton-catchup",
                "skeleton-now-playing",
                "skeleton-stream-clips",
                "skeleton-recent-games",
            ]);

            let target_slots = 9;
            let mut filler_iter = fillers.into_iter();
            while ordered.len() < target_slots {
                if let Some(skel_type) = filler_iter.next() {
                    ordered.push(skeleton_card(skel_type));
                } else {
                    break;
                }
            }

            // Inject invite card unless hidden for this crew
            let active_crew = ctx.app.get_active_crew_id().to_string();
            let invite_hidden = ctx
                .settings
                .borrow()
                .hidden_invite_crew_ids
                .contains(&active_crew);
            if !invite_hidden && !active_crew.is_empty() {
                let mut invite = skeleton_card("invite");
                invite.is_skeleton = false;
                invite.id = "invite".into();
                let insert_pos = 2.min(ordered.len());
                ordered.insert(insert_pos, invite);
            }

            let cards = ordered;
            let is_cold = cards
                .iter()
                .all(|c| c.card_type.starts_with("skeleton") || c.card_type == "invite");

            ctx.app
                .set_feed_cards(Rc::new(slint::VecModel::from(cards)).into());
            ctx.app
                .set_memory_cards(Rc::new(slint::VecModel::from(memory)).into());
            ctx.app.set_feed_cold_start(is_cold);
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
        }
        _ => {}
    }
}
