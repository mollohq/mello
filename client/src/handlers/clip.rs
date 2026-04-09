use std::rc::Rc;

use mello_core::{Command, Event};

use crate::app_context::AppContext;
use crate::converters::make_initials;
use crate::FeedCardData;

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

            // Auto-post clip metadata to backend
            let crew_id = ctx.app.get_active_crew_id().to_string();
            if !crew_id.is_empty() {
                let _ = ctx.cmd_tx.try_send(Command::PostClip {
                    crew_id,
                    clip_id,
                    duration_seconds: duration_seconds as f64,
                    local_path: path,
                });
            }
        }
        Event::ClipCaptureFailed { reason } => {
            log::warn!("clip capture failed: {}", reason);
        }
        Event::ClipPosted { clip_id, event_id } => {
            log::info!("clip posted: clip_id={} event_id={}", clip_id, event_id);

            // Reload the timeline to show the new clip
            let crew_id = ctx.app.get_active_crew_id().to_string();
            if !crew_id.is_empty() {
                let _ = ctx.cmd_tx.try_send(Command::LoadCrewTimeline {
                    crew_id,
                    cursor: None,
                });
            }
        }
        Event::TimelineLoaded { response } => {
            log::info!(
                "timeline loaded for crew {}: {} entries",
                response.crew_id,
                response.entries.len()
            );

            let cards: Vec<FeedCardData> = response
                .entries
                .iter()
                .enumerate()
                .map(|(i, entry)| {
                    let actor_name = entry
                        .data
                        .get("participant_names")
                        .and_then(|v| v.as_array())
                        .and_then(|a| a.first())
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    let actor_display = if actor_name.is_empty() {
                        entry
                            .data
                            .get("streamer_name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("someone")
                            .to_string()
                    } else {
                        actor_name
                    };

                    let duration_secs = entry
                        .data
                        .get("duration_seconds")
                        .and_then(|v| v.as_f64())
                        .or_else(|| {
                            entry
                                .data
                                .get("duration_min")
                                .and_then(|v| v.as_f64())
                                .map(|m| m * 60.0)
                        })
                        .unwrap_or(0.0);

                    let duration_str = if duration_secs > 0.0 {
                        let secs = duration_secs as u32;
                        format!("{}:{:02}", secs / 60, secs % 60)
                    } else {
                        String::new()
                    };

                    let game = entry
                        .data
                        .get("game")
                        .or_else(|| entry.data.get("game_name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    let title = entry
                        .data
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    let clip_path = entry
                        .data
                        .get("local_path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    let participant_count = entry
                        .data
                        .get("participants")
                        .and_then(|v| v.as_array())
                        .map(|a| a.len() as i32)
                        .or_else(|| {
                            entry
                                .data
                                .get("participant_count")
                                .and_then(|v| v.as_i64())
                                .map(|n| n as i32)
                        })
                        .unwrap_or(0);

                    let ts_secs = entry.ts / 1000;
                    let now_secs = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;
                    let ago = now_secs - ts_secs;
                    let timestamp = if ago < 60 {
                        "just now".to_string()
                    } else if ago < 3600 {
                        format!("{}m ago", ago / 60)
                    } else if ago < 86400 {
                        format!("{}h ago", ago / 3600)
                    } else {
                        format!("{}d ago", ago / 86400)
                    };

                    FeedCardData {
                        id: entry.id.clone().into(),
                        card_type: entry.entry_type.clone().into(),
                        title: title.into(),
                        subtitle: Default::default(),
                        timestamp: timestamp.into(),
                        duration: duration_str.into(),
                        actor_name: actor_display.clone().into(),
                        actor_initials: make_initials(&actor_display).into(),
                        game_name: game.into(),
                        participant_count,
                        clip_path: clip_path.into(),
                        is_hero: i == 0 && entry.entry_type == "clip",
                        is_skeleton: false,
                    }
                })
                .collect();

            let is_empty = cards.is_empty();
            ctx.app
                .set_feed_cards(Rc::new(slint::VecModel::from(cards)).into());
            ctx.app.set_feed_cold_start(is_empty);
        }
        _ => {}
    }
}
