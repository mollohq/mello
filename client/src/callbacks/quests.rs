//! The screens behind the actions on the face of a quest gift (crew quests,
//! plans/crew-quests.md section 4): the banner picker here; the invite link
//! and crew emoji follow.

use std::sync::{Arc, Mutex};

use slint::{ComponentHandle, Model};

use crate::app_context::AppContext;
use crate::{BannerGame, BannerPicker, CrewEmojiUi, InviteNamePicker};

/// The banner is saved at the shape of the link card.
pub(crate) const BANNER_FRAME: (u32, u32) = (1200, 630);
/// The server takes a banner JPEG of 400 KB at most (crew_gifts.go).
const BANNER_MAX_BYTES: usize = 400 << 10;
/// Games in the picker: the crew's first, then a search.
const PICKER_GAMES: usize = 6;

/// The JPEG of a chosen banner, between the choice and the save.
type PendingJpeg = Arc<Mutex<Option<Vec<u8>>>>;

pub fn wire(ctx: &AppContext) {
    let pending: PendingJpeg = Arc::new(Mutex::new(None));
    let pending_emoji: PendingJpeg = Arc::new(Mutex::new(None));

    // The actions on the face of a gift. The view closes itself.
    {
        let ctx2 = ctx.clone();
        let pending = pending.clone();
        let pending_emoji = pending_emoji.clone();
        ctx.app
            .global::<crate::SealedUi>()
            .on_quest_action(move |action| match action.as_str() {
                "banner-game" => open_banner_picker(&ctx2, 0, &pending),
                "banner-upload" => {
                    open_banner_picker(&ctx2, 1, &pending);
                    choose_banner_image(&ctx2, &pending);
                }
                "link" => open_invite_name(&ctx2),
                "emoji" => open_emoji_add(&ctx2, &pending_emoji),
                other => log::info!("[gifts] action {other} has no screen yet"),
            });
    }

    let eu = ctx.app.global::<CrewEmojiUi>();
    {
        let ctx2 = ctx.clone();
        let pending = pending_emoji.clone();
        eu.on_add_requested(move || open_emoji_add(&ctx2, &pending));
    }
    {
        let ctx2 = ctx.clone();
        let pending = pending_emoji.clone();
        eu.on_choose_image(move || choose_emoji_image(&ctx2, &pending));
    }
    {
        let ctx2 = ctx.clone();
        let pending = pending_emoji.clone();
        eu.on_save(move || save_emoji(&ctx2, &pending));
    }
    {
        let ctx2 = ctx.clone();
        eu.on_remove(move |name| {
            let _ = ctx2.cmd_tx.send(mello_core::Command::RemoveCrewEmoji {
                crew_id: ctx2.app.get_active_crew_id().to_string(),
                name: name.to_string(),
            });
        });
    }

    let np = ctx.app.global::<InviteNamePicker>();
    {
        let ctx2 = ctx.clone();
        np.on_edited(move |name| check_invite_name(&ctx2, name.as_str()));
    }
    {
        let ctx2 = ctx.clone();
        np.on_save(move || save_invite_name(&ctx2));
    }

    let bp = ctx.app.global::<BannerPicker>();
    {
        let ctx2 = ctx.clone();
        bp.on_search(move |q| {
            let results = search_games(&ctx2, q.as_str());
            ctx2.app
                .global::<BannerPicker>()
                .set_results(slint::ModelRc::new(slint::VecModel::from(results)));
        });
    }
    {
        let ctx2 = ctx.clone();
        let pending = pending.clone();
        bp.on_choose_image(move || choose_banner_image(&ctx2, &pending));
    }
    {
        let ctx2 = ctx.clone();
        bp.on_save(move || save_banner(&ctx2, &pending));
    }
}

fn open_banner_picker(ctx: &AppContext, tab: i32, pending: &PendingJpeg) {
    *pending.lock().expect("banner jpeg lock") = None;
    let bp = ctx.app.global::<BannerPicker>();
    let crew = active_crew(ctx);
    let games = crew_games(ctx);
    // The banner the crew has now is picked; else the first game.
    let picked = crew
        .as_ref()
        .map(|c| c.banner_game.to_string())
        .filter(|g| !g.is_empty())
        .or_else(|| games.first().map(|g| g.id.to_string()))
        .unwrap_or_default();
    bp.set_games(slint::ModelRc::new(slint::VecModel::from(games)));
    bp.set_results(slint::ModelRc::default());
    bp.set_query("".into());
    bp.set_picked_id(picked.into());
    bp.set_image(slint::Image::default());
    bp.set_has_image(false);
    bp.set_image_line("".into());
    bp.set_busy(false);
    bp.set_error("".into());
    bp.set_tab(tab);
    bp.set_open(true);
    log::info!("[gifts] banner picker open, tab {tab}");
}

fn active_crew(ctx: &AppContext) -> Option<crate::CrewData> {
    let id = ctx.app.get_active_crew_id();
    let crews = ctx.app.get_crews();
    (0..crews.row_count())
        .filter_map(|i| crews.row_data(i))
        .find(|c| c.id == id)
}

/// A game of the picker: the art Worker id the server takes, and its art.
fn banner_game(ctx: &AppContext, wire_id: &str, game_id: &str, name: &str) -> Option<BannerGame> {
    let ids = crate::game_icons::worker_ids(wire_id, game_id, name, &ctx.installed_games.borrow());
    let id = ids.first()?.clone();
    let art = crate::game_icons::resolve_tile_art(ctx, wire_id, game_id, name);
    Some(BannerGame {
        id: id.into(),
        name: name.into(),
        art: art.capsule,
        has_art: art.has_capsule,
    })
}

/// The games of the crew, most recent first: the sessions of the feed, the
/// quest step, the games the crew has installed, then the viewer's library.
fn crew_games(ctx: &AppContext) -> Vec<BannerGame> {
    let mut seen: Vec<(String, String)> = Vec::new();
    let mut add = |id: &str, name: &str| {
        if !name.is_empty() && !seen.iter().any(|(_, n)| n.eq_ignore_ascii_case(name)) {
            seen.push((id.to_string(), name.to_string()));
        }
    };
    if let Some(feed) = ctx.last_feed.borrow().as_ref() {
        let z = &feed.zones;
        for e in &z.sessions {
            let s = |k: &str| e.data.get(k).and_then(|v| v.as_str()).unwrap_or("");
            add(s("game_id"), s("game_name"));
        }
        if let Some(s) = z.quests.as_ref().and_then(|q| q.steps.session.as_ref()) {
            add(&s.game_id, &s.game_name);
        }
        for g in &z.installed {
            let s = |k: &str| g.get(k).and_then(|v| v.as_str()).unwrap_or("");
            add(s("game_id"), s("game_name"));
        }
    }
    for g in ctx.installed_games.borrow().iter() {
        add(&g.game_id, &g.name);
    }
    seen.iter()
        .filter_map(|(id, name)| banner_game(ctx, id, id, name))
        .take(PICKER_GAMES)
        .collect()
}

/// Games of the catalogue whose name has the query.
fn search_games(ctx: &AppContext, query: &str) -> Vec<BannerGame> {
    thread_local! {
        static HEAD: Option<mello_core::catalogue::Head> = mello_core::catalogue::Head::bundled();
    }
    let hits: Vec<(String, String)> = HEAD.with(|h| {
        h.as_ref()
            .map(|h| {
                h.search(query, PICKER_GAMES)
                    .into_iter()
                    .map(|e| (format!("igdb-{}", e.igdb_id), e.name.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    });
    hits.iter()
        .filter_map(|(id, name)| banner_game(ctx, id, id, name))
        .collect()
}

/// Crop an image to the banner frame and encode it as a JPEG under the
/// server's limit. The pixels of the preview come back with it.
pub(crate) fn banner_jpeg(bytes: &[u8]) -> Option<(Vec<u8>, Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let cropped = crate::game_icons::crop_cover(img, BANNER_FRAME);
    let sized = image::imageops::resize(
        &cropped,
        BANNER_FRAME.0,
        BANNER_FRAME.1,
        image::imageops::FilterType::Lanczos3,
    );
    let rgb = image::DynamicImage::ImageRgba8(sized.clone()).to_rgb8();
    for quality in [85u8, 75, 65, 55, 45] {
        let mut out = Vec::new();
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
        if enc.encode_image(&rgb).is_err() {
            return None;
        }
        if out.len() <= BANNER_MAX_BYTES {
            let (w, h) = sized.dimensions();
            return Some((out, sized.into_raw(), w, h));
        }
    }
    None
}

/// An `e2e` build reads the image pickers' file from here, without a
/// dialog: a journey has no file dialog to drive. The driver sets it for
/// every app (tools/mello-driver/src/app.ts).
#[cfg(feature = "e2e")]
pub const E2E_PICK_FILE_ENV: &str = "MELLO_E2E_PICK_FILE";

/// Pick one image file: its name and bytes. None when the user cancels.
async fn pick_image(exts: &[&str]) -> Option<(String, Vec<u8>)> {
    #[cfg(feature = "e2e")]
    if let Some(path) = std::env::var_os(E2E_PICK_FILE_ENV) {
        let path = std::path::PathBuf::from(path);
        log::info!("[e2e] the picked file is {}", path.display());
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        return std::fs::read(&path).ok().map(|b| (name, b));
    }
    let file = rfd::AsyncFileDialog::new()
        .add_filter("Images", exts)
        .pick_file()
        .await?;
    Some((file.file_name(), file.read().await))
}

fn choose_banner_image(ctx: &AppContext, pending: &PendingJpeg) {
    let app_weak = ctx.app.as_weak();
    let pending = pending.clone();
    ctx.rt.spawn(async move {
        let Some((name, bytes)) = pick_image(&["jpg", "jpeg", "png"]).await else {
            return;
        };
        let made = if bytes.len() > 20 << 20 {
            None
        } else {
            banner_jpeg(&bytes)
        };
        let ok = made.is_some();
        let preview = made.map(|(jpeg, rgba, w, h)| {
            *pending.lock().expect("banner jpeg lock") = Some(jpeg);
            (rgba, w, h)
        });
        let _ = slint::invoke_from_event_loop(move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let bp = app.global::<BannerPicker>();
            match preview {
                Some((rgba, w, h)) => {
                    bp.set_image(crate::avatar::rgba_to_image(&rgba, w, h));
                    bp.set_has_image(true);
                    bp.set_image_line(name.into());
                    bp.set_error("".into());
                }
                None => {
                    bp.set_error("That file is not an image we can use.".into());
                }
            }
            log::info!("[gifts] banner image chosen: usable={ok}");
        });
    });
}

fn save_banner(ctx: &AppContext, pending: &PendingJpeg) {
    let bp = ctx.app.global::<BannerPicker>();
    let crew_id = ctx.app.get_active_crew_id().to_string();
    if crew_id.is_empty() || bp.get_busy() {
        return;
    }
    let cmd = if bp.get_tab() == 0 {
        let game_id = bp.get_picked_id().to_string();
        if game_id.is_empty() {
            return;
        }
        mello_core::Command::SetCrewBannerGame { crew_id, game_id }
    } else {
        let Some(jpeg) = pending.lock().expect("banner jpeg lock").clone() else {
            return;
        };
        mello_core::Command::UploadCrewBanner { crew_id, jpeg }
    };
    bp.set_busy(true);
    bp.set_error("".into());
    let _ = ctx.cmd_tx.send(cmd);
}

/// The answer to a save: close the picker, or say why it failed.
pub(crate) fn on_gift_result(ctx: &AppContext, gift: &str, failed: Option<&str>) {
    if gift == "emoji" {
        let eu = ctx.app.global::<CrewEmojiUi>();
        if eu.get_busy() {
            eu.set_busy(false);
            match failed {
                Some(reason) => eu.set_error(reason.into()),
                None => eu.set_open(false),
            }
        } else if let Some(reason) = failed {
            // A remove that failed.
            eu.set_error(reason.into());
        }
        return;
    }
    if gift == "link" {
        let np = ctx.app.global::<InviteNamePicker>();
        if np.get_busy() {
            np.set_busy(false);
            match failed {
                Some(reason) => np.set_error(reason.into()),
                None => np.set_open(false),
            }
        }
        return;
    }
    if gift != "banner" {
        return;
    }
    let bp = ctx.app.global::<BannerPicker>();
    if !bp.get_busy() {
        return;
    }
    bp.set_busy(false);
    match failed {
        Some(reason) => bp.set_error(reason.into()),
        None => bp.set_open(false),
    }
}

/// Art arrived: the game tiles of an open picker draw it.
pub(crate) fn refresh_picker_art(ctx: &AppContext) {
    let bp = ctx.app.global::<BannerPicker>();
    if !bp.get_open() {
        return;
    }
    let redo = |m: slint::ModelRc<BannerGame>| -> Vec<BannerGame> {
        m.iter()
            .map(|g| {
                if g.has_art {
                    return g;
                }
                let id = g.id.to_string();
                let art = crate::game_icons::resolve_tile_art(ctx, &id, &id, g.name.as_str());
                BannerGame {
                    art: art.capsule,
                    has_art: art.has_capsule,
                    ..g
                }
            })
            .collect()
    };
    bp.set_games(slint::ModelRc::new(slint::VecModel::from(redo(
        bp.get_games(),
    ))));
    bp.set_results(slint::ModelRc::new(slint::VecModel::from(redo(
        bp.get_results(),
    ))));
}

// ── Gift 2: the custom invite link ──────────────────────────────────────────

/// The name rule of the server (crew_gifts.go `inviteNameAllowed`), so the
/// screen refuses a bad name without a call. The server checks again.
pub(crate) fn invite_name_ok(name: &str) -> bool {
    const NOT_ALLOWED: [&str; 15] = [
        "m3llo",
        "mello",
        "mollo",
        "admin",
        "support",
        "help",
        "api",
        "join",
        "invite",
        "official",
        "staff",
        "mod",
        "moderator",
        "system",
        "lounge",
    ];
    let b = name.as_bytes();
    let form = (3..=24).contains(&b.len())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--");
    // The shape of a generated code, with or without its dash.
    let compact = name.replacen('-', "", 1);
    let code_shape = compact.len() == 8
        && compact.bytes().all(|c| c.is_ascii_alphanumeric())
        && (!name.contains('-') || name.find('-') == Some(4));
    form && !code_shape && !NOT_ALLOWED.contains(&name)
}

fn open_invite_name(ctx: &AppContext) {
    let np = ctx.app.global::<InviteNamePicker>();
    let crew = active_crew(ctx);
    let current = crew
        .as_ref()
        .map(|c| c.invite_name.to_string())
        .unwrap_or_default();
    let suggested = ctx
        .app
        .global::<crate::SealedUi>()
        .get_quest()
        .gift_text
        .to_string();
    let name = if current.is_empty() {
        suggested
    } else {
        current
    };
    np.set_crew_name(crew.map(|c| c.name).unwrap_or_default());
    np.set_name(name.clone().into());
    np.set_busy(false);
    np.set_error("".into());
    np.set_open(true);
    log::info!("[gifts] invite name screen open");
    check_invite_name(ctx, &name);
}

fn check_invite_name(ctx: &AppContext, raw: &str) {
    let np = ctx.app.global::<InviteNamePicker>();
    let name = raw.trim().to_lowercase();
    if !invite_name_ok(&name) {
        np.set_state("not_allowed".into());
        return;
    }
    np.set_state("checking".into());
    let _ = ctx.cmd_tx.send(mello_core::Command::CheckInviteName {
        crew_id: ctx.app.get_active_crew_id().to_string(),
        name,
    });
}

/// The answer to a check, when it is for the name in the field now.
pub(crate) fn on_invite_name_checked(ctx: &AppContext, name: &str, state: &str) {
    let np = ctx.app.global::<InviteNamePicker>();
    if np.get_open() && np.get_name().trim().to_lowercase() == name {
        np.set_state(if state.is_empty() { "" } else { state }.into());
    }
}

fn save_invite_name(ctx: &AppContext) {
    let np = ctx.app.global::<InviteNamePicker>();
    let name = np.get_name().trim().to_lowercase();
    if np.get_busy() || np.get_state() != "free" || !invite_name_ok(&name) {
        return;
    }
    np.set_busy(true);
    np.set_error("".into());
    let _ = ctx.cmd_tx.send(mello_core::Command::SetInviteName {
        crew_id: ctx.app.get_active_crew_id().to_string(),
        name,
    });
}

// ── Gift 3: crew emoji ──────────────────────────────────────────────────────

/// A crew emoji is sent square, at 128 px, as a PNG (it keeps transparency).
const EMOJI_SIDE: u32 = 128;
/// The server takes an emoji of 256 KB at most (crew_gifts.go).
const EMOJI_MAX_BYTES: usize = 256 << 10;

/// Crop an image square and encode it at the emoji size.
pub(crate) fn emoji_png(bytes: &[u8]) -> Option<(Vec<u8>, Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let square = crate::game_icons::crop_cover(img, (1, 1));
    let sized = image::imageops::resize(
        &square,
        EMOJI_SIDE,
        EMOJI_SIDE,
        image::imageops::FilterType::Lanczos3,
    );
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(sized.clone())
        .write_to(&mut out, image::ImageFormat::Png)
        .ok()?;
    let png = out.into_inner();
    (png.len() <= EMOJI_MAX_BYTES).then(|| (png, sized.into_raw(), EMOJI_SIDE, EMOJI_SIDE))
}

fn open_emoji_add(ctx: &AppContext, pending: &PendingJpeg) {
    *pending.lock().expect("emoji png lock") = None;
    let eu = ctx.app.global::<CrewEmojiUi>();
    eu.set_name("".into());
    eu.set_image(slint::Image::default());
    eu.set_has_image(false);
    eu.set_busy(false);
    eu.set_error("".into());
    eu.set_open(true);
    log::info!("[gifts] crew emoji screen open");
}

fn choose_emoji_image(ctx: &AppContext, pending: &PendingJpeg) {
    let app_weak = ctx.app.as_weak();
    let pending = pending.clone();
    ctx.rt.spawn(async move {
        let Some((_, bytes)) = pick_image(&["png", "jpg", "jpeg", "gif"]).await else {
            return;
        };
        let made = if bytes.len() > 20 << 20 {
            None
        } else {
            emoji_png(&bytes)
        };
        let preview = made.map(|(png, rgba, w, h)| {
            *pending.lock().expect("emoji png lock") = Some(png);
            (rgba, w, h)
        });
        let _ = slint::invoke_from_event_loop(move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let eu = app.global::<CrewEmojiUi>();
            match preview {
                Some((rgba, w, h)) => {
                    eu.set_image(crate::avatar::rgba_to_image(&rgba, w, h));
                    eu.set_has_image(true);
                    eu.set_error("".into());
                }
                None => eu.set_error("That file is not an image we can use.".into()),
            }
        });
    });
}

/// The emoji name rule of the server: 2 to 24 lower-case letters, digits or
/// underscores.
pub(crate) fn emoji_name_ok(name: &str) -> bool {
    crate::converters::crew_emoji_token(&format!(":{name}:")).is_some()
}

fn save_emoji(ctx: &AppContext, pending: &PendingJpeg) {
    let eu = ctx.app.global::<CrewEmojiUi>();
    let name = eu.get_name().trim().trim_matches(':').to_lowercase();
    if eu.get_busy() {
        return;
    }
    if !emoji_name_ok(&name) {
        eu.set_error("A name is 2 to 24 letters, digits or underscores.".into());
        return;
    }
    let Some(png) = pending.lock().expect("emoji png lock").clone() else {
        return;
    };
    eu.set_busy(true);
    eu.set_error("".into());
    let _ = ctx.cmd_tx.send(mello_core::Command::UploadCrewEmoji {
        crew_id: ctx.app.get_active_crew_id().to_string(),
        name,
        mime: "image/png".into(),
        bytes: png,
    });
}
