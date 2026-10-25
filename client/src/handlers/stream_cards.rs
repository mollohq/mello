use std::rc::Rc;

use crate::app_context::AppContext;
use crate::converters::{avatar_color_index, make_initials};
use crate::StreamCardData;

pub fn sync_active_stream_cards(ctx: &AppContext) {
    let mut cards: Vec<StreamCardData> = Vec::new();
    // Stage 9: the live counts. The hosted card counts the viewers that
    // mello-core reports; the watched card counts the SFU membership and
    // falls back to the crew state's count (the backend's `viewer_ids`)
    // until the first snapshot.
    let backend_count = ctx.app.get_active_stream_viewer_count();
    let (host_viewers, watched_viewers) = {
        let state = ctx.stream_card.borrow();
        (state.host_viewers.len(), state.watched_viewers)
    };

    if ctx.app.get_is_hosting() {
        let host_id = ctx.app.get_user_id().to_string();
        let host_name = ctx.app.get_user_name().to_string();
        let host_avatar = ctx.app.get_user_avatar();
        let host_has_avatar = ctx.app.get_has_user_avatar();
        let stream_label = ctx.app.get_stream_label().to_string();
        cards.push(StreamCardData {
            host_id: host_id.clone().into(),
            session_id: "".into(),
            streamer_name: host_name.clone().into(),
            subtitle: stream_label.into(),
            avatar: host_avatar,
            has_avatar: host_has_avatar,
            avatar_initials: make_initials(&host_name).into(),
            avatar_color_index: avatar_color_index(&host_id),
            // The live viewers: mello-core reports each join and leave.
            viewer_count: host_viewers as i32,
            stream_width: 0,
            stream_height: 0,
            can_watch: false,
            state: ctx.app.get_stream_host_state(),
            title: "".into(),
        });
    }

    let active_host_id = ctx.app.get_active_streamer_id().to_string();
    if !active_host_id.is_empty() {
        let active_streamer_name = ctx.app.get_active_streamer_name().to_string();
        let active_stream_title = ctx.app.get_active_stream_title().to_string();
        let (avatar, has_avatar) = if let Some(img) = ctx.avatar_cache.borrow().get(&active_host_id)
        {
            (img.clone(), true)
        } else {
            (slint::Image::default(), false)
        };
        cards.push(StreamCardData {
            host_id: active_host_id.clone().into(),
            session_id: ctx.app.get_active_stream_session_id(),
            streamer_name: active_streamer_name.clone().into(),
            subtitle: if active_stream_title.is_empty() {
                "Live now".into()
            } else {
                active_stream_title.into()
            },
            avatar,
            has_avatar,
            avatar_initials: make_initials(&active_streamer_name).into(),
            avatar_color_index: avatar_color_index(&active_host_id),
            viewer_count: match watched_viewers {
                Some(n) if ctx.app.get_is_watching() => n as i32,
                _ => backend_count,
            },
            stream_width: ctx.app.get_active_stream_width(),
            stream_height: ctx.app.get_active_stream_height(),
            can_watch: true,
            state: if ctx.app.get_is_watching() {
                ctx.app.get_stream_viewer_state()
            } else {
                "".into()
            },
            title: "".into(),
        });
    }

    // Stage 6: a stream that ended or did not start keeps a card for a few
    // seconds that says why (the hosted one, or the watched one).
    let notice_state = ctx.app.get_stream_notice_state().to_string();
    if !notice_state.is_empty() {
        let name = ctx.app.get_stream_notice_name().to_string();
        let id = format!("notice-{name}");
        cards.push(StreamCardData {
            host_id: id.clone().into(),
            session_id: "".into(),
            streamer_name: name.clone().into(),
            subtitle: ctx.app.get_stream_notice(),
            avatar: slint::Image::default(),
            has_avatar: false,
            avatar_initials: make_initials(&name).into(),
            avatar_color_index: avatar_color_index(&id),
            viewer_count: 0,
            stream_width: 0,
            stream_height: 0,
            can_watch: false,
            state: notice_state.into(),
            title: ctx.app.get_stream_notice_title(),
        });
    }

    // Stage 9: the expanded view shows the watchable card.
    {
        use slint::ComponentHandle as _;
        let watched = cards
            .iter()
            .find(|c| c.can_watch)
            .cloned()
            .unwrap_or_default();
        ctx.app
            .global::<crate::StreamCardUi>()
            .set_watched_card(watched);
    }
    ctx.app
        .set_active_stream_cards(Rc::new(slint::VecModel::from(cards)).into());
}
