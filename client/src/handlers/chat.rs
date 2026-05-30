use std::rc::Rc;

use mello_core::{Command, Event};
use slint::{ComponentHandle, Model};

use crate::app_context::AppContext;
use crate::converters::{
    apply_unread_to_crews, chat_messages_to_slint, fetch_gif_images_for_messages,
    member_names_from_app, ChatConvertOptions,
};
use crate::{image_cache, notifications, CrewData, GifItemData};

fn refresh_chat_ui(ctx: &AppContext) {
    let uid = ctx.app.get_user_id().to_string();
    let uav = ctx.app.get_user_avatar();
    let huav = ctx.app.get_has_user_avatar();
    let member_names = member_names_from_app(&ctx.app);
    let first_unread = ctx.chat_scroll.first_unread_id();
    let opts = ChatConvertOptions {
        user_id: &uid,
        user_avatar: &uav,
        has_user_avatar: huav,
        avatar_cache: &ctx.avatar_cache.borrow(),
        member_names: &member_names,
        first_unread_id: first_unread.as_deref(),
    };
    let raw = ctx.chat_messages.borrow();
    let display = chat_messages_to_slint(&raw, &opts);
    let rc = Rc::new(slint::VecModel::from(display));
    ctx.app.set_messages(rc.clone().into());
    ctx.app
        .set_chat_messages_this_week(mello_core::chat::count_messages_this_week(&raw));
    fetch_gif_images_for_messages(&rc, &ctx.rt, &ctx.gif_chat_anim);
    ctx.chat_scroll.apply_to_window(&ctx.app);
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
                    .try_send(Command::FetchUserAvatars { user_ids: uncached });
            }
            *ctx.chat_messages.borrow_mut() = messages;
            ctx.app.set_has_more_history(has_more_history);
            ctx.chat_scroll.reset_on_messages_loaded();
            let active = ctx.app.get_active_crew_id().to_string();
            if !active.is_empty() {
                ctx.unread_tracker.borrow_mut().reset(&active);
                apply_unread_to_crews(&ctx.app, &ctx.unread_tracker.borrow());
            }
            refresh_chat_ui(ctx);
        }
        Event::MessageReceived { message } => {
            if !ctx.app.window().is_visible() {
                let crew_name = ctx.app.get_active_crew_id().to_string();
                notifications::notify_message(&crew_name, &message.sender_name, &message.content);
            }
            let sender_id = message.sender_id.clone();
            let message_id = message.message_id.clone();
            let uid = ctx.app.get_user_id().to_string();
            let is_own = sender_id == uid;

            let member_names = member_names_from_app(&ctx.app);
            let mentions_self =
                mello_core::chat::prepare_body_for_display(&message.content, &uid, &member_names).1;

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

            ctx.chat_scroll.on_incoming_message(is_own, &message_id);
            upsert_message(ctx, message);
            refresh_chat_ui(ctx);

            if sender_id != uid && !ctx.avatar_cache.borrow().contains_key(&sender_id) {
                let _ = ctx
                    .cmd_tx
                    .try_send(Command::FetchUserAvatar { user_id: sender_id });
            }
        }
        Event::HistoryLoaded { messages, cursor } => {
            let prepended = messages.len();
            let mut all = messages;
            all.append(&mut ctx.chat_messages.borrow().clone());
            *ctx.chat_messages.borrow_mut() = all;
            ctx.app.set_has_more_history(cursor.is_some());
            ctx.chat_scroll.on_history_prepended(prepended);
            ctx.app.set_loading_history(false);
            refresh_chat_ui(ctx);
        }
        Event::HistoryLoadFailed => {
            ctx.app.set_loading_history(false);
        }
        Event::ChatMessageEdited {
            message_id,
            new_content,
            update_time,
        } => {
            log::info!("Message edited: {} at {}", message_id, update_time);
            {
                let mut msgs = ctx.chat_messages.borrow_mut();
                if let Some(m) = msgs.iter_mut().find(|m| m.message_id == message_id) {
                    m.content = new_content;
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
                    m.is_deleted = true;
                    m.is_edited = false;
                }
            }
            refresh_chat_ui(ctx);
        }
        Event::GifsLoaded { gifs } => {
            log::info!("[gif] loaded {} results", gifs.len());
            ctx.gif_popover_anim.stop_and_clear();

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
            ctx.gif_popover_anim.start(move |url, img| {
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

            let inbox = ctx.gif_popover_anim.inbox();
            for g in &gifs {
                image_cache::spawn_gif_fetch(g.preview.clone(), &ctx.rt, &inbox);
            }
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
            ctx.app
                .set_crews(Rc::new(slint::VecModel::from(updated)).into());
        }
        _ => {}
    }
}
