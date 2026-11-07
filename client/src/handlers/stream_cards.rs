use std::rc::Rc;

use crate::app_context::AppContext;
use crate::converters::{avatar_color_index, make_initials};
use crate::StreamCardData;
use slint::ComponentHandle;

/// The game of a stream, for the card: the catalogue entry of the igdb id
/// the host sent, its icon-slot art from the shared tile ladder, and the
/// hero poster when the game is on this disk.
#[derive(Default)]
struct StreamGame {
    id: String,
    name: String,
    monogram: String,
    hue: i32,
    icon: slint::Image,
    has_icon: bool,
    poster: Option<slint::Image>,
}

fn stream_game(ctx: &AppContext, igdb_id: i32) -> StreamGame {
    if igdb_id <= 0 {
        return StreamGame::default();
    }
    thread_local! {
        static CATALOGUE: std::cell::OnceCell<Option<mello_core::catalogue::Head>> =
            const { std::cell::OnceCell::new() };
    }
    let entry = CATALOGUE.with(|head| {
        let head = head.get_or_init(mello_core::catalogue::Head::bundled);
        head.as_ref()
            .and_then(|h| h.get(igdb_id as u32))
            .map(|e| (e.game_id.to_string(), e.name.to_string()))
    });
    let Some((id, name)) = entry else {
        log::debug!("[stream-card] igdb_id={igdb_id} is not in the bundled catalogue");
        return StreamGame::default();
    };
    let wire = format!("igdb-{igdb_id}");
    let art = crate::game_icons::resolve_tile_art_slot(
        ctx,
        &wire,
        &id,
        &name,
        mello_core::art::ArtSlot::Icon,
    );
    let poster = crate::game_icons::resolve_poster(ctx, &wire, &id, &name);
    StreamGame {
        monogram: crate::recent_games::monogram(&name),
        hue: crate::recent_games::hue_index(&id, &name),
        id,
        name,
        icon: art.icon,
        has_icon: art.has_icon,
        poster,
    }
}

/// What fills the card before you watch: the SFU's last picture when it is
/// the one of this stream, else the game's hero art, else nothing.
fn stream_picture(
    ctx: &AppContext,
    game: &StreamGame,
    frame: bool,
) -> (slint::Image, &'static str) {
    if frame {
        let url = ctx.app.get_active_stream_thumbnail_url();
        if !url.is_empty() && url == ctx.app.get_active_stream_picture_url() {
            return (ctx.app.get_active_stream_picture(), "frame");
        }
    }
    match &game.poster {
        Some(p) => (p.clone(), "poster"),
        None => (slint::Image::default(), ""),
    }
}

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
        let game = stream_game(ctx, ctx.app.get_stream_igdb_id());
        let (picture, picture_kind) = stream_picture(ctx, &game, false);
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
            game_id: game.id.into(),
            game_name: game.name.into(),
            game_monogram: game.monogram.into(),
            game_hue_index: game.hue,
            game_icon: game.icon,
            has_game_icon: game.has_icon,
            picture,
            picture_kind: picture_kind.into(),
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
        let active_igdb_id = ctx.app.get_active_stream_igdb_id();
        let game = stream_game(ctx, active_igdb_id);
        let (picture, picture_kind) = stream_picture(ctx, &game, true);
        log::debug!(
            "[stream-card] watchable card of {}: igdb_id={} catalogue={} icon={} picture={:?}",
            active_host_id,
            active_igdb_id,
            if game.id.is_empty() { "miss" } else { "hit" },
            game.has_icon,
            picture_kind
        );
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
            game_id: game.id.into(),
            game_name: game.name.into(),
            game_monogram: game.monogram.into(),
            game_hue_index: game.hue,
            game_icon: game.icon,
            has_game_icon: game.has_icon,
            picture,
            picture_kind: picture_kind.into(),
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
            ..Default::default()
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
    // Two or three streams and you have joined none: strips, not a grid.
    let strips = !ctx.app.get_is_watching()
        && (2..=3).contains(&cards.len())
        && cards.iter().all(|c| c.can_watch && c.state.is_empty());
    ctx.app
        .global::<crate::StreamCardUi>()
        .set_strips_layout(strips);
    // A live stream that you do not watch shows its SFU picture.
    let poll =
        !ctx.app.get_is_watching() && cards.iter().any(|c| c.can_watch && c.state.is_empty());
    ctx.app
        .set_active_stream_cards(Rc::new(slint::VecModel::from(cards)).into());
    crate::stream_picture::sync(ctx, poll);
    // A live stream takes the Stage.
    super::zones::refresh(ctx);
}
