use std::cell::RefCell;
use std::rc::Rc;

use mello_core::chat::MentionRef;
use mello_core::Command;
use slint::{ComponentHandle, Model};

use crate::app_context::AppContext;
use crate::converters::apply_unread_to_crews;
use crate::{EmojiItemData, MentionMemberData};

/// One emoji per category as the tab's icon, in `CATEGORIES` order.
const CATEGORY_ICONS: [&str; 8] = ["😀", "🐱", "🍕", "⚽", "🚗", "💡", "❤️", "🏳️"];
/// Cells per picker row.
const EMOJI_COLUMNS: usize = 9;
/// Recent emoji kept: one row.
const RECENT_EMOJI: usize = EMOJI_COLUMNS;

fn emoji_item(e: &mello_core::emoji::EmojiEntry) -> EmojiItemData {
    EmojiItemData {
        emoji: e.emoji.into(),
        name: e.name.into(),
    }
}

/// The picker grid as rows of nine.
fn emoji_rows(entries: &[mello_core::emoji::EmojiEntry]) -> slint::ModelRc<crate::EmojiRowData> {
    let rows: Vec<crate::EmojiRowData> = entries
        .chunks(EMOJI_COLUMNS)
        .map(|chunk| crate::EmojiRowData {
            cells: Rc::new(slint::VecModel::from(
                chunk.iter().map(emoji_item).collect::<Vec<_>>(),
            ))
            .into(),
        })
        .collect();
    Rc::new(slint::VecModel::from(rows)).into()
}

fn emoji_categories() -> slint::ModelRc<crate::EmojiCategoryData> {
    let cats: Vec<crate::EmojiCategoryData> = mello_core::emoji::CATEGORIES
        .iter()
        .zip(CATEGORY_ICONS.iter())
        .map(|(name, icon)| crate::EmojiCategoryData {
            name: (*name).into(),
            icon: (*icon).into(),
        })
        .collect();
    Rc::new(slint::VecModel::from(cats)).into()
}

/// The picker: the full set by category, search by name, and a Recent row
/// kept for the session.
fn wire_emoji_picker(ctx: &AppContext) {
    ctx.app.set_emoji_categories(emoji_categories());
    ctx.app
        .set_emoji_rows(emoji_rows(&mello_core::emoji::emoji_in_category(0)));
    ctx.app.set_emoji_category(0);
    {
        let app_weak = ctx.app.as_weak();
        ctx.app.on_emoji_search(move |query| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let entries = if query.trim().is_empty() {
                mello_core::emoji::emoji_in_category(app.get_emoji_category().max(0) as usize)
            } else {
                mello_core::emoji::search_emoji(query.as_str())
            };
            app.set_emoji_rows(emoji_rows(&entries));
        });
    }
    {
        let app_weak = ctx.app.as_weak();
        ctx.app.on_emoji_category_selected(move |index| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let index = (index.max(0) as usize).min(mello_core::emoji::CATEGORIES.len() - 1);
            app.set_emoji_category(index as i32);
            app.set_emoji_rows(emoji_rows(&mello_core::emoji::emoji_in_category(index)));
        });
    }
    {
        let app_weak = ctx.app.as_weak();
        let recent: Rc<RefCell<Vec<String>>> = Rc::default();
        ctx.app.on_emoji_picked(move |emoji| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let mut list = recent.borrow_mut();
            list.retain(|e| e != emoji.as_str());
            list.insert(0, emoji.to_string());
            list.truncate(RECENT_EMOJI);
            let all = mello_core::emoji::all_emoji();
            let items: Vec<EmojiItemData> = list
                .iter()
                .map(|e| EmojiItemData {
                    emoji: e.clone().into(),
                    name: all
                        .iter()
                        .find(|x| x.emoji == e.as_str())
                        .map(|x| x.name)
                        .unwrap_or("emoji")
                        .into(),
                })
                .collect();
            app.set_recent_emoji(Rc::new(slint::VecModel::from(items)).into());
        });
    }
}

pub fn refresh_mention_members(ctx: &AppContext) {
    let members = ctx.app.get_members();
    let list: Vec<MentionMemberData> = (0..members.row_count())
        .filter_map(|i| members.row_data(i))
        .map(|m| MentionMemberData {
            user_id: m.id.clone(),
            display_name: m.name.clone(),
            initials: m.initials.clone(),
        })
        .collect();
    ctx.app
        .set_mention_members(Rc::new(slint::VecModel::from(list)).into());
}

/// The members picked from mention autocomplete since the last send.
type MentionPicks = Rc<RefCell<Vec<MentionRef>>>;

/// The picks whose `@name` is still in `text`. The picks are cleared, so the
/// next message starts with none.
fn take_picks(picks: &MentionPicks, text: &str) -> Vec<MentionRef> {
    std::mem::take(&mut *picks.borrow_mut())
        .into_iter()
        .filter(|p| text.contains(&format!("@{}", p.name)))
        .collect()
}

/// Composer text after a pick: the `@query` being typed becomes `@name `.
fn text_after_pick(current: &str, name: &str) -> String {
    let head = current.rfind('@').map_or(current, |at| &current[..at]);
    format!("{head}@{name} ")
}

/// Adds one member to the mention picker if missing. Channel presence (a member
/// opening the app) only ever adds: the picker lists the whole crew, online or
/// not, and is rebuilt from the crew roster when crew state loads.
pub fn add_mention_member(ctx: &AppContext, user_id: &str, display_name: &str, initials: &str) {
    let current = ctx.app.get_mention_members();
    if (0..current.row_count())
        .filter_map(|i| current.row_data(i))
        .any(|m| m.user_id == user_id)
    {
        return;
    }
    let mut list: Vec<MentionMemberData> = (0..current.row_count())
        .filter_map(|i| current.row_data(i))
        .collect();
    list.push(MentionMemberData {
        user_id: user_id.into(),
        display_name: display_name.into(),
        initials: initials.into(),
    });
    ctx.app
        .set_mention_members(Rc::new(slint::VecModel::from(list)).into());
}

pub fn wire(ctx: &AppContext) {
    wire_emoji_picker(ctx);
    refresh_mention_members(ctx);
    let picks: MentionPicks = Rc::default();

    {
        let cmd = ctx.cmd_tx.clone();
        let scroll_ctx = Rc::new(ChatSendScroll {
            scroll: ctx.chat_scroll.clone(),
            unread: ctx.unread_tracker.clone(),
            app: ctx.app.as_weak(),
        });
        ctx.app.on_send_message({
            let cmd = cmd.clone();
            let s = scroll_ctx.clone();
            let picks = picks.clone();
            let composer = ctx.chat_attachments.clone();
            let app_weak = ctx.app.as_weak();
            move |text| {
                let mentions = take_picks(&picks, &text);
                submit_composer(&cmd, &composer, &app_weak, text.to_string(), None, mentions);
                s.fire();
            }
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let scroll_ctx = Rc::new(ChatSendScroll {
            scroll: ctx.chat_scroll.clone(),
            unread: ctx.unread_tracker.clone(),
            app: ctx.app.as_weak(),
        });
        let picks = picks.clone();
        let composer = ctx.chat_attachments.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_send_message_with_reply(move |text, reply_to| {
            let mentions = take_picks(&picks, &text);
            submit_composer(
                &cmd,
                &composer,
                &app_weak,
                text.to_string(),
                Some(reply_to.to_string()),
                mentions,
            );
            scroll_ctx.fire();
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        let refresh = crate::handlers::chat::ChatRefresh::new(ctx);
        ctx.app.on_request_history(move || {
            // Messages trimmed from the bottom of the window are still loaded.
            // Show those first; ask the server only past the oldest loaded one.
            if refresh
                .scroll()
                .reveal_local_history(refresh.loaded_count())
                > 0
            {
                refresh.run();
                return;
            }
            if let Some(a) = app_weak.upgrade() {
                a.set_loading_history(true);
            }
            let _ = cmd.send(Command::LoadHistory { cursor: None });
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let scroll_ctx = Rc::new(ChatSendScroll {
            scroll: ctx.chat_scroll.clone(),
            unread: ctx.unread_tracker.clone(),
            app: ctx.app.as_weak(),
        });
        let picks = picks.clone();
        let chat_messages = ctx.chat_messages.clone();
        ctx.app.on_edit_message(move |message_id, new_body| {
            // The edit box shows `@name`, so the message's own mentions count as picks.
            let existing: Vec<MentionRef> = chat_messages
                .borrow()
                .iter()
                .find(|m| m.message_id == message_id.as_str())
                .map(|m| m.mentions.clone())
                .unwrap_or_default();
            picks.borrow_mut().extend(existing);
            let _ = cmd.send(Command::EditMessage {
                mentions: take_picks(&picks, &new_body),
                message_id: message_id.to_string(),
                new_body: new_body.to_string(),
            });
            scroll_ctx.fire();
        });
    }
    // "Say hi" on a joined card: a greeting that mentions the new member goes
    // to the crew chat at once, once for each member.
    {
        let cmd = ctx.cmd_tx.clone();
        let settings = ctx.settings.clone();
        let last_feed = ctx.last_feed.clone();
        let app_weak = ctx.app.as_weak();
        let scroll_ctx = Rc::new(ChatSendScroll {
            scroll: ctx.chat_scroll.clone(),
            unread: ctx.unread_tracker.clone(),
            app: ctx.app.as_weak(),
        });
        ctx.app
            .global::<crate::ChatFocus>()
            .on_say_hi(move |card_id| {
                let Some(app) = app_weak.upgrade() else {
                    return;
                };
                let Some(user_id) = crate::say_hi::user_of_card(card_id.as_str()) else {
                    return;
                };
                let crew_id = app.get_active_crew_id().to_string();
                // The member joined in the last 24 hours (`zones.people`).
                let Some(join_name) = last_feed.borrow().as_ref().and_then(|f| {
                    f.zones
                        .people
                        .joined
                        .iter()
                        .find(|j| j.user_id == user_id)
                        .map(|j| j.name.clone())
                }) else {
                    return;
                };
                // The roster name first: the join event keeps the name of then.
                let name = app
                    .get_members()
                    .iter()
                    .find(|m| m.id.as_str() == user_id)
                    .map(|m| m.name.to_string())
                    .filter(|n| !n.trim().is_empty())
                    .unwrap_or(join_name);
                if crew_id.is_empty() || name.trim().is_empty() {
                    return;
                }
                {
                    let mut s = settings.borrow_mut();
                    let said = s.said_hi.entry(crew_id.clone()).or_default();
                    if said.iter().any(|u| u == user_id) {
                        return;
                    }
                    said.push(user_id.to_string());
                    s.save();
                }
                let (content, mention) = crate::say_hi::greeting(user_id, &name);
                log::info!("[feed] said hi to {user_id} in crew {crew_id}");
                let _ = cmd.send(Command::SendMessage {
                    content,
                    reply_to: None,
                    mentions: vec![mention],
                });
                scroll_ctx.fire();
                // The button reads "Said hi" and goes off.
                let mut z = app.get_feed_zones();
                if z.say_hi_card == card_id {
                    z.said_hi = true;
                    app.set_feed_zones(z);
                }
            });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let messages = ctx.chat_messages.clone();
        ctx.app.on_delete_message(move |message_id| {
            // The objects of an image message go with it. The server accepts
            // only the caller's own keys, and only the sender can delete.
            let urls: Vec<String> = messages
                .borrow()
                .iter()
                .find(|m| m.message_id == message_id.as_str())
                .map(|m| m.images.iter().map(|i| i.url.clone()).collect())
                .unwrap_or_default();
            let _ = cmd.send(Command::DeleteMessage {
                message_id: message_id.to_string(),
            });
            if !urls.is_empty() {
                let _ = cmd.send(Command::DeleteChatImages { urls });
            }
        });
    }
    wire_attachments(ctx);
    {
        let cmd = ctx.cmd_tx.clone();
        let reactions = ctx.reactions.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_react(move |message_id, emoji| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let uid = app.get_user_id().to_string();
            let mine = reactions
                .borrow()
                .get(message_id.as_str())
                .and_then(|m| m.get(emoji.as_str()))
                .is_some_and(|users| users.contains(&uid));
            let _ = cmd.send(Command::ReactToMessage {
                message_id: message_id.to_string(),
                emoji: emoji.to_string(),
                add: !mine,
            });
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_gif_pill_clicked(move || {
            log::info!("[gif] pill clicked, loading trending");
            let _ = cmd.send(Command::LoadTrendingGifs);
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_gif_search(move |query| {
            let q = query.to_string();
            if q.is_empty() {
                let _ = cmd.send(Command::LoadTrendingGifs);
            } else {
                let _ = cmd.send(Command::SearchGifs { query: q });
            }
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let scroll_ctx = Rc::new(ChatSendScroll {
            scroll: ctx.chat_scroll.clone(),
            unread: ctx.unread_tracker.clone(),
            app: ctx.app.as_weak(),
        });
        ctx.app
            .on_gif_selected(move |gif_id, url, preview_url, _w, _h| {
                log::info!("[gif] selected id={}", gif_id.as_str());
                let gif = mello_core::chat::GifData {
                    id: gif_id.to_string(),
                    url: url.to_string(),
                    preview: preview_url.to_string(),
                    width: _w as u32,
                    height: _h as u32,
                    alt: String::new(),
                };
                let _ = cmd.send(Command::SendGif {
                    gif,
                    body: String::new(),
                });
                scroll_ctx.fire();
            });
    }
    {
        let anim = ctx.gif_chat_anim.clone();
        let rt = ctx.rt.clone();
        ctx.app.on_gif_hovered(move |url| {
            // A GIF past the prefetch window, or one evicted from the cache,
            // loads on hover. A paused one plays again.
            if anim.is_requested(url.as_str()) {
                anim.resume(url.as_str());
            } else {
                anim.fetch(url.as_str(), &rt);
            }
        });
    }
    {
        let scroll = ctx.chat_scroll.clone();
        let unread = ctx.unread_tracker.clone();
        let app_weak = ctx.app.as_weak();
        let read = MarkRead::new(ctx);
        ctx.app.on_chat_viewport_changed(move |at_bottom| {
            scroll.on_viewport_at_bottom(at_bottom);
            if at_bottom {
                read.mark();
                if let Some(app) = app_weak.upgrade() {
                    let active = app.get_active_crew_id().to_string();
                    if !active.is_empty() {
                        unread.borrow_mut().reset(&active);
                        apply_unread_to_crews(&app, &unread.borrow());
                    }
                    app.set_has_new_messages(false);
                }
            }
        });
    }
    {
        let scroll = ctx.chat_scroll.clone();
        ctx.app.on_scroll_to_bottom_done(move || {
            scroll.pending_scroll_to_bottom.set(false);
        });
    }
    {
        let app_weak = ctx.app.as_weak();
        ctx.app.on_history_prepended_done(move || {
            if let Some(app) = app_weak.upgrade() {
                app.set_history_prepended_rows(0);
            }
        });
    }
    {
        ctx.app.on_link_clicked(|url| {
            crate::open_url::open(url.as_str());
        });
    }
    {
        ctx.app.on_copy_message_text(|text| {
            if let Ok(mut clipboard) = arboard::Clipboard::new() {
                let _ = clipboard.set_text(text.as_str());
            }
        });
    }
    {
        let app_weak = ctx.app.as_weak();
        let picks = picks.clone();
        ctx.app.on_mention_selected(move |user_id, display_name| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            // Show the name; the core writes the `<@user_id>` token at send time.
            let current = app.get_chat_input_text();
            app.set_chat_input_text(text_after_pick(&current, &display_name).into());
            picks.borrow_mut().push(MentionRef {
                user_id: user_id.to_string(),
                name: display_name.to_string(),
            });
            app.set_mention_suggestions(
                Rc::new(slint::VecModel::<MentionMemberData>::default()).into(),
            );
        });
    }
    {
        let ctx_members = ctx.app.as_weak();
        ctx.app.on_chat_input_changed(move |text| {
            let Some(app) = ctx_members.upgrade() else {
                return;
            };
            let multiline = text.contains('\n');
            app.set_composer_multiline(multiline);
            let line_count = if text.is_empty() {
                1
            } else {
                text.split('\n').count().max(1) as i32
            };
            app.set_composer_line_count(line_count.min(5));
            if !multiline && line_count <= 1 {
                app.set_composer_line_count(1);
            }

            let members = app.get_mention_members();
            let Some(at_pos) = text.rfind('@') else {
                app.set_mention_suggestions(
                    Rc::new(slint::VecModel::<MentionMemberData>::default()).into(),
                );
                return;
            };
            let after = &text[at_pos + 1..];
            if after.contains(' ') || after.contains('\n') {
                app.set_mention_suggestions(
                    Rc::new(slint::VecModel::<MentionMemberData>::default()).into(),
                );
                return;
            }
            let q = after.to_lowercase();
            let suggestions: Vec<MentionMemberData> = (0..members.row_count())
                .filter_map(|i| members.row_data(i))
                .filter(|m| {
                    let name = m.display_name.to_string().to_lowercase();
                    name.starts_with(&q) || m.user_id.to_string().to_lowercase().starts_with(&q)
                })
                .take(8)
                .collect();
            app.set_mention_suggestions(Rc::new(slint::VecModel::from(suggestions)).into());
        });
    }
}

struct ChatSendScroll {
    scroll: Rc<crate::chat_ui::ChatScrollState>,
    unread: Rc<std::cell::RefCell<mello_core::chat::UnreadTracker>>,
    app: slint::Weak<crate::MainWindow>,
}

impl ChatSendScroll {
    fn fire(&self) {
        self.scroll.request_scroll_to_bottom();
        if let Some(app) = self.app.upgrade() {
            let active = app.get_active_crew_id().to_string();
            if !active.is_empty() {
                self.unread.borrow_mut().reset(&active);
                apply_unread_to_crews(&app, &self.unread.borrow());
            }
            app.set_scroll_to_bottom_request(true);
            app.set_has_new_messages(false);
        }
    }
}

// ---------------------------------------------------------------------------
// Image attachments: chips, upload, paste, the viewer
// ---------------------------------------------------------------------------

use crate::chat_images::{
    lock, prepare_image, ready_images, Attachment, AttachmentState, Attachments, PendingSend,
    PreparedImage, MAX_ATTACHMENTS,
};

/// The chip rows for the window, from the composer state.
fn attachment_rows(composer: &Attachments) -> Vec<crate::AttachmentData> {
    lock(composer)
        .attachments
        .iter()
        .map(|a| crate::AttachmentData {
            id: a.id.clone().into(),
            state: a.state_name().into(),
            label: a.label().into(),
            thumb: a
                .thumb
                .as_ref()
                .map(|(rgba, w, h)| {
                    slint::Image::from_rgba8(
                        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                            rgba, *w, *h,
                        ),
                    )
                })
                .unwrap_or_default(),
            has_thumb: a.thumb.is_some(),
        })
        .collect()
}

fn push_attachment_rows(app_weak: &slint::Weak<crate::MainWindow>, composer: &Attachments) {
    if let Some(app) = app_weak.upgrade() {
        app.set_chat_attachments(Rc::new(slint::VecModel::from(attachment_rows(composer))).into());
    }
}

/// Redraw the chips from the composer state.
pub(crate) fn sync_attachments(ctx: &AppContext) {
    push_attachment_rows(&ctx.app.as_weak(), &ctx.chat_attachments);
}

/// Send now, or when every upload has settled. Text alone goes as a text
/// message. Ready images go with the text as one image message.
fn submit_composer(
    cmd: &tokio::sync::mpsc::UnboundedSender<Command>,
    composer: &Attachments,
    app_weak: &slint::Weak<crate::MainWindow>,
    text: String,
    reply_to: Option<String>,
    mentions: Vec<MentionRef>,
) {
    let has_chips = !lock(composer).attachments.is_empty();
    if !has_chips {
        if text.is_empty() {
            return;
        }
        let _ = cmd.send(Command::SendMessage {
            mentions,
            content: text,
            reply_to,
        });
        return;
    }
    lock(composer).pending_send = Some(PendingSend {
        text,
        reply_to,
        mentions,
    });
    flush_pending_send_with(cmd, composer, app_weak);
}

/// The send that waits on uploads, once nothing is uploading. Failed chips
/// stay, so the user sees why, unless the message went out without them.
pub(crate) fn flush_pending_send(ctx: &AppContext) {
    flush_pending_send_with(&ctx.cmd_tx, &ctx.chat_attachments, &ctx.app.as_weak());
}

fn flush_pending_send_with(
    cmd: &tokio::sync::mpsc::UnboundedSender<Command>,
    composer: &Attachments,
    app_weak: &slint::Weak<crate::MainWindow>,
) {
    let (pending, images) = {
        let state = lock(composer);
        let Some(pending) = state.pending_send.clone() else {
            return;
        };
        let Some(images) = ready_images(&state.attachments) else {
            return; // still uploading; ChatImageUploaded calls again
        };
        (pending, images)
    };
    lock(composer).pending_send = None;
    if images.is_empty() {
        // Every chip failed. Text alone still goes; the chips stay with
        // their reasons.
        if !pending.text.is_empty() {
            let _ = cmd.send(Command::SendMessage {
                mentions: pending.mentions,
                content: pending.text,
                reply_to: pending.reply_to,
            });
        }
        return;
    }
    let _ = cmd.send(Command::SendImages {
        images,
        content: pending.text,
        reply_to: pending.reply_to,
        mentions: pending.mentions,
    });
    lock(composer).attachments.clear();
    if let Some(app) = app_weak.upgrade() {
        app.set_chat_attachments(
            Rc::new(slint::VecModel::<crate::AttachmentData>::default()).into(),
        );
    }
}

/// What the prepare task needs back on the UI thread, with the window weak.
struct AttachmentRefresh {
    app: slint::Weak<crate::MainWindow>,
    composer: Attachments,
    cmd: tokio::sync::mpsc::UnboundedSender<Command>,
}

impl AttachmentRefresh {
    fn prepared(&self, id: &str, result: Result<PreparedImage, crate::chat_images::PrepareError>) {
        {
            let mut state = lock(&self.composer);
            let Some(a) = state.attachments.iter_mut().find(|a| a.id == id) else {
                return; // removed while preparing
            };
            match &result {
                Ok(p) => {
                    a.thumb = Some(p.thumb.clone());
                    a.bytes = p.bytes.len() as u64;
                    a.state = AttachmentState::Uploading;
                }
                Err(e) => a.state = AttachmentState::Failed(e.0.clone()),
            }
        }
        if let Ok(p) = result {
            let _ = self.cmd.send(Command::UploadChatImage {
                attachment_id: id.to_string(),
                mime: p.mime.to_string(),
                width: p.width,
                height: p.height,
                bytes: p.bytes,
            });
        }
        push_attachment_rows(&self.app, &self.composer);
        flush_pending_send_with(&self.cmd, &self.composer, &self.app);
    }
}

fn wire_attachments(ctx: &AppContext) {
    {
        let rt = ctx.rt.clone();
        let composer = ctx.chat_attachments.clone();
        let app_weak = ctx.app.as_weak();
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_attach_clicked(move || {
            let room = MAX_ATTACHMENTS.saturating_sub(lock(&composer).attachments.len());
            if room == 0 {
                return;
            }
            let composer = composer.clone();
            let app_weak = app_weak.clone();
            let cmd = cmd.clone();
            let rt2 = rt.clone();
            rt.spawn(async move {
                let files = rfd::AsyncFileDialog::new()
                    .add_filter("Images", &["jpg", "jpeg", "png", "gif"])
                    .pick_files()
                    .await
                    .unwrap_or_default();
                for file in files.into_iter().take(room) {
                    let bytes = file.read().await;
                    let composer = composer.clone();
                    let app_weak = app_weak.clone();
                    let cmd = cmd.clone();
                    let rt3 = rt2.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        add_attachment_with(&composer, &app_weak, &cmd, &rt3, bytes);
                    });
                }
            });
        });
    }
    {
        let composer = ctx.chat_attachments.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_attachment_removed(move |id| {
            lock(&composer).attachments.retain(|a| a.id != id.as_str());
            push_attachment_rows(&app_weak, &composer);
        });
    }
    {
        let composer = ctx.chat_attachments.clone();
        let app_weak = ctx.app.as_weak();
        let cmd = ctx.cmd_tx.clone();
        let rt = ctx.rt.clone();
        ctx.app.on_paste_image(move || {
            let Some(png) = clipboard_image_png() else {
                return false;
            };
            add_attachment_with(&composer, &app_weak, &cmd, &rt, png);
            true
        });
    }
    {
        let messages = ctx.chat_messages.clone();
        let anim = ctx.gif_chat_anim.clone();
        let app_weak = ctx.app.as_weak();
        let viewer = ViewerFetch::new(ctx);
        ctx.app.on_chat_image_clicked(move |url, sender| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            // The viewer shows every image of the message, starting at the
            // one clicked, with the feed's small pixels until the full ones land.
            let images: Vec<mello_core::chat::ImageData> = messages
                .borrow()
                .iter()
                .find(|m| m.images.iter().any(|i| i.url == url.as_str()))
                .map(|m| m.images.clone())
                .unwrap_or_default();
            if images.is_empty() {
                return;
            }
            let index = images
                .iter()
                .position(|i| i.url == url.as_str())
                .unwrap_or(0);
            let rows: Vec<crate::ViewerImageData> = images
                .iter()
                .map(|i| {
                    let size = if i.bytes > 0 {
                        format!(" · {}", crate::chat_images::size_label(i.bytes))
                    } else {
                        String::new()
                    };
                    let small = anim.current_frame(&i.url);
                    crate::ViewerImageData {
                        url: i.url.clone().into(),
                        width: i.width as i32,
                        height: i.height as i32,
                        label: format!("{}×{}{}", i.width, i.height, size).into(),
                        has_image: small.is_some(),
                        image: small.unwrap_or_default(),
                        failed: anim.has_failed(&i.url),
                    }
                })
                .collect();
            app.set_chat_viewer_images(Rc::new(slint::VecModel::from(rows)).into());
            app.set_chat_viewer_index(index as i32);
            app.set_chat_viewer_sender(sender);
            app.set_chat_viewer_open(true);
            viewer.show(index);
        });
    }
    {
        let viewer = ViewerFetch::new(ctx);
        ctx.app.on_chat_viewer_shown(move |index| {
            viewer.show(index.max(0) as usize);
        });
    }
    {
        let rt = ctx.rt.clone();
        ctx.app.on_chat_image_save(move |url| {
            let url = url.to_string();
            rt.spawn(async move {
                let name = url.rsplit('/').next().unwrap_or("image").to_string();
                let Some(target) = rfd::AsyncFileDialog::new()
                    .set_file_name(&name)
                    .save_file()
                    .await
                else {
                    return;
                };
                match crate::http::shared().get(&url).send().await {
                    Ok(resp) => match resp.bytes().await {
                        Ok(bytes) => match std::fs::write(target.path(), &bytes) {
                            Ok(()) => {
                                log::info!("[chat] image saved to {}", target.path().display())
                            }
                            Err(e) => log::warn!("[chat] image save failed: {}", e),
                        },
                        Err(e) => log::warn!("[chat] image download failed: {}", e),
                    },
                    Err(e) => log::warn!("[chat] image download failed: {}", e),
                }
            });
        });
    }
    {
        let rt = ctx.rt.clone();
        ctx.app.on_chat_image_copy(move |url| {
            let url = url.to_string();
            rt.spawn(async move {
                let bytes = match crate::http::shared().get(&url).send().await {
                    Ok(resp) => match resp.bytes().await {
                        Ok(b) => b,
                        Err(e) => {
                            log::warn!("[chat] image download failed: {}", e);
                            return;
                        }
                    },
                    Err(e) => {
                        log::warn!("[chat] image download failed: {}", e);
                        return;
                    }
                };
                let Ok(img) = image::load_from_memory(&bytes) else {
                    log::warn!("[chat] image copy: cannot decode");
                    return;
                };
                let rgba = img.to_rgba8();
                let (w, h) = (rgba.width() as usize, rgba.height() as usize);
                let raw = rgba.into_raw();
                // The clipboard belongs to the UI thread.
                let _ = slint::invoke_from_event_loop(move || {
                    let data = arboard::ImageData {
                        width: w,
                        height: h,
                        bytes: std::borrow::Cow::Owned(raw),
                    };
                    match arboard::Clipboard::new().and_then(|mut c| c.set_image(data)) {
                        Ok(()) => log::info!("[chat] image copied"),
                        Err(e) => log::warn!("[chat] image copy failed: {}", e),
                    }
                });
            });
        });
    }
}

/// Add a chip for `source` and prepare it off the UI thread. When prepared,
/// the chip gets its thumbnail and the bytes go to the core for upload.
fn add_attachment_with(
    composer: &Attachments,
    app_weak: &slint::Weak<crate::MainWindow>,
    cmd: &tokio::sync::mpsc::UnboundedSender<Command>,
    rt: &tokio::runtime::Handle,
    source: Vec<u8>,
) {
    let Some(id) = begin_attachment(composer, app_weak) else {
        return;
    };
    let refresh = AttachmentRefresh {
        app: app_weak.clone(),
        composer: composer.clone(),
        cmd: cmd.clone(),
    };
    rt.spawn(async move {
        let prepared = prepare_image(&source);
        let _ = slint::invoke_from_event_loop(move || refresh.prepared(&id, prepared));
    });
}

/// Push a "preparing" chip and draw it. `None` when the four slots are full.
pub(crate) fn begin_attachment(
    composer: &Attachments,
    app_weak: &slint::Weak<crate::MainWindow>,
) -> Option<String> {
    let id = uuid::Uuid::new_v4().to_string();
    {
        let mut state = lock(composer);
        if state.attachments.len() >= MAX_ATTACHMENTS {
            log::info!(
                "[chat] attachment ignored: {} chips already",
                MAX_ATTACHMENTS
            );
            return None;
        }
        state.attachments.push(Attachment {
            id: id.clone(),
            state: AttachmentState::Preparing,
            thumb: None,
            bytes: 0,
        });
    }
    push_attachment_rows(app_weak, composer);
    Some(id)
}

/// The UI-thread half of a prepare: what the task hands back. Tests call it
/// with the result of `prepare_image` instead of running the task.
#[cfg(test)]
pub(crate) fn attachment_prepared(
    ctx: &AppContext,
    id: &str,
    result: Result<PreparedImage, crate::chat_images::PrepareError>,
) {
    AttachmentRefresh {
        app: ctx.app.as_weak(),
        composer: ctx.chat_attachments.clone(),
        cmd: ctx.cmd_tx.clone(),
    }
    .prepared(id, result);
}

/// The clipboard's image as PNG bytes, if it holds one.
fn clipboard_image_png() -> Option<Vec<u8>> {
    let data = arboard::Clipboard::new().ok()?.get_image().ok()?;
    let img = image::RgbaImage::from_raw(
        data.width as u32,
        data.height as u32,
        data.bytes.into_owned(),
    )?;
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .ok()?;
    Some(out.into_inner())
}

/// What marking the crew on screen as read needs, with the window weak.
struct MarkRead {
    app: slint::Weak<crate::MainWindow>,
    messages: Rc<RefCell<Vec<mello_core::events::ChatMessage>>>,
    scroll: Rc<crate::chat_ui::ChatScrollState>,
    cursors: Rc<RefCell<std::collections::HashMap<String, mello_core::chat::ChatReadCursor>>>,
    cmd: tokio::sync::mpsc::UnboundedSender<Command>,
}

impl MarkRead {
    fn new(ctx: &AppContext) -> Self {
        Self {
            app: ctx.app.as_weak(),
            messages: ctx.chat_messages.clone(),
            scroll: ctx.chat_scroll.clone(),
            cursors: ctx.read_cursors.clone(),
            cmd: ctx.cmd_tx.clone(),
        }
    }

    /// Same rule as `handlers::chat::mark_read`: the newest message, once.
    fn mark(&self) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let crew = app.get_active_crew_id().to_string();
        if crew.is_empty() {
            return;
        }
        let newest = self
            .messages
            .borrow()
            .iter()
            .rev()
            .find(|m| !m.is_system)
            .map(|m| (m.message_id.clone(), m.create_time.clone()));
        let Some((message_id, time)) = newest else {
            return;
        };
        {
            let mut last = self.scroll.last_marked.borrow_mut();
            if last
                .as_ref()
                .is_some_and(|(c, id)| *c == crew && *id == message_id)
            {
                return;
            }
            *last = Some((crew.clone(), message_id.clone()));
        }
        self.cursors.borrow_mut().insert(
            crew.clone(),
            mello_core::chat::ChatReadCursor {
                message_id: message_id.clone(),
                time: time.clone(),
            },
        );
        let _ = self.cmd.send(Command::MarkChatRead {
            crew_id: crew,
            message_id,
            time,
        });
    }
}

/// Fetches the full image for the viewer's shown index, once per URL, and
/// puts it into the viewer's row. The feed cache holds a 640px version; the
/// viewer wants the real one.
struct ViewerFetch {
    app: slint::Weak<crate::MainWindow>,
    rt: tokio::runtime::Handle,
    fetched: Rc<RefCell<std::collections::HashSet<String>>>,
}

impl ViewerFetch {
    fn new(ctx: &AppContext) -> Self {
        Self {
            app: ctx.app.as_weak(),
            rt: ctx.rt.clone(),
            fetched: Rc::default(),
        }
    }

    fn show(&self, index: usize) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let rows = app.get_chat_viewer_images();
        let Some(row) = rows.row_data(index) else {
            return;
        };
        let url = row.url.to_string();
        if url.is_empty() || !self.fetched.borrow_mut().insert(url.clone()) {
            return;
        }
        let app_weak = self.app.clone();
        self.rt.spawn(async move {
            let decoded = match crate::http::shared().get(&url).send().await {
                Ok(resp) => match resp.bytes().await {
                    Ok(bytes) => image::load_from_memory(&bytes).ok().map(|img| {
                        // Bounded, so a seeded or pasted monster never costs a GPU texture
                        // the size of a wall.
                        let img = if img.width().max(img.height()) > 2048 {
                            img.resize(2048, 2048, image::imageops::FilterType::Triangle)
                        } else {
                            img
                        };
                        let rgba = img.to_rgba8();
                        let (w, h) = (rgba.width(), rgba.height());
                        (rgba.into_raw(), w, h)
                    }),
                    Err(_) => None,
                },
                Err(_) => None,
            };
            let _ = slint::invoke_from_event_loop(move || {
                let Some(app) = app_weak.upgrade() else {
                    return;
                };
                let rows = app.get_chat_viewer_images();
                for i in 0..rows.row_count() {
                    let Some(mut row) = rows.row_data(i) else {
                        continue;
                    };
                    if row.url.as_str() != url {
                        continue;
                    }
                    match &decoded {
                        Some((rgba, w, h)) => {
                            row.image = slint::Image::from_rgba8(slint::SharedPixelBuffer::<
                                slint::Rgba8Pixel,
                            >::clone_from_slice(
                                rgba, *w, *h
                            ));
                            row.has_image = true;
                            row.failed = false;
                        }
                        None => {
                            if !row.has_image {
                                row.failed = true;
                            }
                        }
                    }
                    rows.set_row_data(i, row);
                }
            });
        });
    }
}
