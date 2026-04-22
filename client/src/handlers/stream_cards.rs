use std::rc::Rc;

use crate::app_context::AppContext;
use crate::converters::make_initials;
use crate::StreamCardData;

fn avatar_color_index(seed: &str) -> i32 {
    if seed.is_empty() {
        return 0;
    }

    (seed.bytes().fold(0u32, |acc, b| acc.wrapping_add(b as u32)) % 5) as i32
}

pub fn sync_active_stream_cards(ctx: &AppContext) {
    let mut cards: Vec<StreamCardData> = Vec::new();

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
            viewer_count: ctx.app.get_active_stream_viewer_count(),
            stream_width: 0,
            stream_height: 0,
            can_watch: false,
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
            viewer_count: ctx.app.get_active_stream_viewer_count(),
            stream_width: ctx.app.get_active_stream_width(),
            stream_height: ctx.app.get_active_stream_height(),
            can_watch: true,
        });
    }

    ctx.app
        .set_active_stream_cards(Rc::new(slint::VecModel::from(cards)).into());
}
