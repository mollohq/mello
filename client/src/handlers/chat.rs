use std::rc::Rc;

use mello_core::{Command, Event};
use slint::{ComponentHandle, Model};

use crate::app_context::AppContext;
use crate::converters::{
    apply_unread_to_crews, chat_messages_to_slint, color_hex, sync_chat_rows, sync_gif_images,
    ChatConvertOptions,
};
use crate::gif_animator::GifFrame;
use crate::{notifications, CrewData, GifItemData};

/// Rebuild the shown rows from the loaded messages and the scroll state.
/// Shows the window `ChatScrollState` describes, and changes the existing
/// row model in place so Slint keeps the rows that did not change.
pub(crate) fn refresh_chat_ui(ctx: &AppContext) {
    ChatRefresh::new(ctx).run();
}

/// What a row rebuild needs, with the window held weakly. A Slint callback
/// that captured a strong `AppContext` would own the window that owns the
/// callback, and the window would never drop (see `Harness::drop`).
pub(crate) struct ChatRefresh {
    app: slint::Weak<crate::MainWindow>,
    chat_messages: Rc<std::cell::RefCell<Vec<mello_core::events::ChatMessage>>>,
    chat_scroll: Rc<crate::chat_ui::ChatScrollState>,
    avatar_cache: Rc<std::cell::RefCell<std::collections::HashMap<String, slint::Image>>>,
    reactions: crate::converters::ReactionMap,
    crew_emoji: Rc<std::cell::RefCell<Vec<mello_core::crew_state::CrewEmoji>>>,
    media_images: Rc<std::cell::RefCell<std::collections::HashMap<String, Option<slint::Image>>>>,
    rt: tokio::runtime::Handle,
    gif_chat_anim: crate::gif_animator::GifAnimator,
}

impl ChatRefresh {
    pub(crate) fn new(ctx: &AppContext) -> Self {
        Self {
            app: ctx.app.as_weak(),
            chat_messages: ctx.chat_messages.clone(),
            chat_scroll: ctx.chat_scroll.clone(),
            avatar_cache: ctx.avatar_cache.clone(),
            reactions: ctx.reactions.clone(),
            crew_emoji: ctx.crew_emoji.clone(),
            media_images: ctx.media_images.clone(),
            rt: ctx.rt.clone(),
            gif_chat_anim: ctx.gif_chat_anim.clone(),
        }
    }

    /// The loaded messages, for a caller that decides before it refreshes.
    pub(crate) fn loaded_count(&self) -> usize {
        self.chat_messages.borrow().len()
    }

    pub(crate) fn scroll(&self) -> &crate::chat_ui::ChatScrollState {
        &self.chat_scroll
    }

    pub(crate) fn run(&self) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let uid = app.get_user_id().to_string();
        let uav = app.get_user_avatar();
        let huav = app.get_has_user_avatar();
        let mention_color = color_hex(app.global::<crate::Theme>().get_mention());
        let first_unread = self.chat_scroll.first_unread_id();
        let reactions = self.reactions.borrow();
        // The loaded crew emoji by name. One that is not loaded yet draws as
        // its text; the load builds the rows again.
        let crew_emoji: std::collections::HashMap<String, slint::Image> = {
            let media = self.media_images.borrow();
            self.crew_emoji
                .borrow()
                .iter()
                .filter_map(|e| {
                    media
                        .get(&e.url)
                        .and_then(|i| i.clone())
                        .map(|i| (e.name.clone(), i))
                })
                .collect()
        };
        let opts = ChatConvertOptions {
            user_id: &uid,
            reactions: &reactions,
            user_avatar: &uav,
            has_user_avatar: huav,
            avatar_cache: &self.avatar_cache.borrow(),
            crew_emoji: &crew_emoji,
            mention_color: &mention_color,
            first_unread_id: first_unread.as_deref(),
        };
        let raw = self.chat_messages.borrow();
        let start = self.chat_scroll.window_start(raw.len());
        let display = chat_messages_to_slint(&raw[start..], &opts);
        sync_chat_rows(&app, display);
        app.set_chat_messages_this_week(mello_core::chat::count_messages_this_week(&raw));
        sync_gif_images(&app.get_messages(), &self.rt, &self.gif_chat_anim);
        self.chat_scroll.apply_to_window(&app, raw.len());
    }
}

/// Move one pending chip to `state` and redraw the chips. A chip the user
/// removed while its upload ran is gone, and its result is dropped.
fn set_attachment_state(
    ctx: &AppContext,
    attachment_id: &str,
    state: crate::chat_images::AttachmentState,
) {
    {
        let mut composer = crate::chat_images::lock(&ctx.chat_attachments);
        let Some(a) = composer
            .attachments
            .iter_mut()
            .find(|a| a.id == attachment_id)
        else {
            return;
        };
        a.state = state;
    }
    crate::callbacks::chat::sync_attachments(ctx);
    crate::callbacks::chat::flush_pending_send(ctx);
}

/// Store the read cursor at the newest message of the crew on screen, once
/// per message. Called when the user is at the bottom.
pub(crate) fn mark_read(ctx: &AppContext) {
    let crew = ctx.app.get_active_crew_id().to_string();
    if crew.is_empty() {
        return;
    }
    let newest = ctx
        .chat_messages
        .borrow()
        .iter()
        .rev()
        .find(|m| !m.is_system)
        .map(|m| (m.message_id.clone(), m.create_time.clone()));
    let Some((message_id, time)) = newest else {
        return;
    };
    {
        let mut last = ctx.chat_scroll.last_marked.borrow_mut();
        if last
            .as_ref()
            .is_some_and(|(c, id)| *c == crew && *id == message_id)
        {
            return;
        }
        *last = Some((crew.clone(), message_id.clone()));
    }
    ctx.read_cursors.borrow_mut().insert(
        crew.clone(),
        mello_core::chat::ChatReadCursor {
            message_id: message_id.clone(),
            time: time.clone(),
        },
    );
    let _ = ctx.cmd_tx.send(Command::MarkChatRead {
        crew_id: crew,
        message_id,
        time,
    });
}

/// The sidebar badges from the read cursors and the newest messages of
/// each crew. The crew on screen has no badge; its unread rule is in the feed.
pub(crate) fn recompute_badges(ctx: &AppContext) {
    let active = ctx.app.get_active_crew_id().to_string();
    let uid = ctx.app.get_user_id().to_string();
    let cursors = ctx.read_cursors.borrow();
    let previews = ctx.crew_previews.borrow();
    let mut tracker = ctx.unread_tracker.borrow_mut();
    for (crew, list) in previews.iter() {
        if *crew == active {
            continue;
        }
        let unread = list
            .iter()
            .filter(|p| p.user_id.as_deref() != Some(uid.as_str()))
            .filter(|p| match cursors.get(crew) {
                Some(c) => {
                    p.message_id.as_deref() != Some(c.message_id.as_str())
                        && mello_core::chat::is_after(&p.timestamp, &c.time)
                }
                None => true,
            })
            .count() as u32;
        let had_mention = tracker.get(crew).has_mention;
        tracker.set(crew, unread, had_mention && unread > 0);
    }
    drop(tracker);
    apply_unread_to_crews(&ctx.app, &ctx.unread_tracker.borrow());
}

fn upsert_message(ctx: &AppContext, message: mello_core::events::ChatMessage) {
    let mut msgs = ctx.chat_messages.borrow_mut();
    if let Some(existing) = msgs.iter_mut().find(|m| m.message_id == message.message_id) {
        *existing = message;
    } else {
        msgs.push(message);
    }
}

pub fn handle(ctx: &AppContext, event: Event) {
    match event {
        Event::MessagesLoaded {
            messages,
            has_more_history,
        } => {
            let uid = ctx.app.get_user_id().to_string();
            let uncached: Vec<String> = {
                let cache = ctx.avatar_cache.borrow();
                messages
                    .iter()
                    .map(|m| m.sender_id.clone())
                    .filter(|sid| *sid != uid && !cache.contains_key(sid))
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect()
            };
            if !uncached.is_empty() {
                let _ = ctx
                    .cmd_tx
                    .send(Command::FetchUserAvatars { user_ids: uncached });
            }
            *ctx.chat_messages.borrow_mut() = messages;
            ctx.reactions.borrow_mut().clear();
            ctx.chat_scroll.reset_on_messages_loaded(has_more_history);
            let active = ctx.app.get_active_crew_id().to_string();
            if !active.is_empty() {
                ctx.unread_tracker.borrow_mut().reset(&active);
                apply_unread_to_crews(&ctx.app, &ctx.unread_tracker.borrow());
            }
            refresh_chat_ui(ctx);
        }
        Event::ChatReactionsLoaded { crew_id, items } => {
            if crew_id != ctx.app.get_active_crew_id().as_str() {
                return;
            }
            {
                let mut map = ctx.reactions.borrow_mut();
                for item in items {
                    if item.reactions.is_empty() {
                        map.remove(&item.message_id);
                    } else {
                        map.insert(item.message_id, item.reactions);
                    }
                }
            }
            refresh_chat_ui(ctx);
        }
        Event::ChatReactionChanged { update } => {
            if update.crew_id != ctx.app.get_active_crew_id().as_str() {
                return;
            }
            {
                let mut map = ctx.reactions.borrow_mut();
                if update.reactions.is_empty() {
                    map.remove(&update.message_id);
                } else {
                    map.insert(update.message_id, update.reactions);
                }
            }
            refresh_chat_ui(ctx);
        }
        Event::ChatReadCursorLoaded { crew_id, cursor } => {
            if let Some(c) = cursor.clone() {
                ctx.read_cursors.borrow_mut().insert(crew_id.clone(), c);
            }
            if crew_id != ctx.app.get_active_crew_id().as_str() {
                return;
            }
            let first_unread = cursor.as_ref().and_then(|c| {
                mello_core::chat::first_unread_after(&ctx.chat_messages.borrow(), c)
                    .map(|m| m.message_id.clone())
            });
            match first_unread {
                Some(id) => {
                    log::info!("[chat] opening {} at the unread rule, {}", crew_id, id);
                    ctx.chat_scroll.open_at_unread(&id);
                    refresh_chat_ui(ctx);
                }
                None => mark_read(ctx),
            }
        }
        Event::ChatReadCursors { cursors } => {
            ctx.read_cursors.borrow_mut().extend(cursors);
            recompute_badges(ctx);
        }
        Event::MessageReceived { message } => {
            if !ctx.app.window().is_visible() {
                let crew_name = ctx.app.get_active_crew_id().to_string();
                notifications::notify_message(
                    &crew_name,
                    &message.sender_name,
                    &message.display_body,
                );
            }
            let sender_id = message.sender_id.clone();
            let message_id = message.message_id.clone();
            let uid = ctx.app.get_user_id().to_string();
            let is_own = sender_id == uid;

            let mentions_self = message.mentions_user(&uid);

            let active_crew = ctx.app.get_active_crew_id().to_string();
            let was_at_bottom = ctx.chat_scroll.at_bottom.get();
            if !is_own && !was_at_bottom && !active_crew.is_empty() {
                ctx.unread_tracker
                    .borrow_mut()
                    .increment(&active_crew, mentions_self);
                apply_unread_to_crews(&ctx.app, &ctx.unread_tracker.borrow());
            } else if (is_own || was_at_bottom) && !active_crew.is_empty() {
                ctx.unread_tracker.borrow_mut().reset(&active_crew);
                apply_unread_to_crews(&ctx.app, &ctx.unread_tracker.borrow());
            }

            let scrolls = ctx.chat_scroll.on_incoming_message(is_own, &message_id);
            upsert_message(ctx, message);
            refresh_chat_ui(ctx);
            if scrolls {
                mark_read(ctx);
            }

            if sender_id != uid && !ctx.avatar_cache.borrow().contains_key(&sender_id) {
                let _ = ctx
                    .cmd_tx
                    .send(Command::FetchUserAvatar { user_id: sender_id });
            }
        }
        Event::HistoryLoaded { messages, cursor } => {
            // A page can overlap what is loaded (a message that arrived while
            // the page was in flight). Keep one copy per id.
            let (prepended, has_more) = {
                let mut all = ctx.chat_messages.borrow_mut();
                let known: std::collections::HashSet<String> =
                    all.iter().map(|m| m.message_id.clone()).collect();
                let mut fresh: Vec<_> = messages
                    .into_iter()
                    .filter(|m| !known.contains(&m.message_id))
                    .collect();
                let prepended = fresh.len();
                fresh.append(&mut all);
                *all = fresh;
                (prepended, cursor.is_some())
            };
            ctx.chat_scroll.on_server_history(prepended, has_more);
            ctx.app.set_loading_history(false);
            refresh_chat_ui(ctx);
        }
        Event::HistoryLoadFailed => {
            ctx.app.set_loading_history(false);
        }
        Event::ChatMessageEdited {
            message_id,
            new_content,
            new_display_body,
            mentions,
            update_time,
        } => {
            log::info!("Message edited: {} at {}", message_id, update_time);
            {
                let mut msgs = ctx.chat_messages.borrow_mut();
                if let Some(m) = msgs.iter_mut().find(|m| m.message_id == message_id) {
                    m.content = new_content;
                    m.display_body = new_display_body;
                    m.mentions = mentions;
                    m.update_time = update_time;
                    m.is_edited = true;
                    m.is_deleted = false;
                }
            }
            refresh_chat_ui(ctx);
        }
        Event::ChatMessageDeleted { message_id } => {
            log::info!("Message deleted: {}", message_id);
            {
                let mut msgs = ctx.chat_messages.borrow_mut();
                if let Some(m) = msgs.iter_mut().find(|m| m.message_id == message_id) {
                    m.content.clear();
                    m.display_body.clear();
                    m.mentions.clear();
                    m.is_deleted = true;
                    m.is_edited = false;
                }
            }
            refresh_chat_ui(ctx);
        }
        Event::GifsLoaded { gifs } => {
            log::info!("[gif] loaded {} results", gifs.len());
            ctx.gif_popover_anim.stop_and_clear();
            ctx.app.set_gif_error(slint::SharedString::default());

            let model: Vec<GifItemData> = gifs
                .iter()
                .map(|g| GifItemData {
                    gif_id: g.id.clone().into(),
                    url: g.url.clone().into(),
                    preview_url: g.preview.clone().into(),
                    width: g.width as i32,
                    height: g.height as i32,
                    preview: slint::Image::default(),
                    has_preview: false,
                })
                .collect();
            let vec_model = Rc::new(slint::VecModel::from(model));
            ctx.app.set_gif_results(vec_model.clone().into());

            let app_weak = ctx.app.as_weak();
            ctx.gif_popover_anim.start(move |url, frame| {
                let GifFrame::Image(img) = frame else {
                    // A tile that never loads stays a blank cell; the rest
                    // of the page is still usable.
                    return;
                };
                let Some(app) = app_weak.upgrade() else {
                    return;
                };
                let model = app.get_gif_results();
                for i in 0..model.row_count() {
                    if let Some(mut item) = model.row_data(i) {
                        if item.preview_url.as_str() == url {
                            item.preview = img.clone();
                            item.has_preview = true;
                            model.set_row_data(i, item);
                            break;
                        }
                    }
                }
            });

            for g in &gifs {
                ctx.gif_popover_anim.fetch(&g.preview, &ctx.rt);
            }
        }
        Event::GifsFailed { error } => {
            use mello_core::giphy::GifError;
            log::warn!("[gif] request failed: {}", error);
            ctx.gif_popover_anim.stop_and_clear();
            ctx.app
                .set_gif_results(Rc::new(slint::VecModel::<GifItemData>::default()).into());
            let text = match error {
                GifError::NoKey => "GIFs are off in this build.",
                GifError::Unauthorized => "The GIF service refused this build's key.",
                GifError::RateLimited => "Too many GIF searches right now. Try again in a minute.",
                GifError::Status(_) | GifError::Network(_) | GifError::Parse(_) => {
                    "The GIF service did not answer. Your messages still send."
                }
            };
            ctx.app.set_gif_error(text.into());
        }
        Event::ChatImageUploaded {
            attachment_id,
            image,
        } => {
            log::info!("[chat] attachment {} ready: {}", attachment_id, image.url);
            set_attachment_state(
                ctx,
                &attachment_id,
                crate::chat_images::AttachmentState::Ready(image),
            );
        }
        Event::ChatImageUploadFailed {
            attachment_id,
            reason,
        } => {
            set_attachment_state(
                ctx,
                &attachment_id,
                crate::chat_images::AttachmentState::Failed(reason),
            );
        }
        Event::MessagePreviewUpdated { crew_id, messages } => {
            log::debug!(
                "UI: message preview for crew={} count={}",
                crew_id,
                messages.len()
            );
            let current = ctx.app.get_crews();
            let mut updated: Vec<CrewData> = (0..current.row_count())
                .map(|i| current.row_data(i).unwrap())
                .collect();
            ctx.crew_previews
                .borrow_mut()
                .insert(crew_id.clone(), messages.clone());
            if let Some(c) = updated.iter_mut().find(|c| c.id == crew_id.as_str()) {
                c.msg_count = messages.len().min(2) as i32;
                if let Some(m) = messages.first() {
                    c.m0_author = m.username.clone().into();
                    c.m0_text = m.preview.clone().into();
                }
                if let Some(m) = messages.get(1) {
                    c.m1_author = m.username.clone().into();
                    c.m1_text = m.preview.clone().into();
                }
            }
            crate::converters::sync_crews(&ctx.app, updated);
            recompute_badges(ctx);
        }
        _ => {}
    }
}
