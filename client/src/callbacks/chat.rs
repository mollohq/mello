use std::cell::RefCell;
use std::rc::Rc;

use mello_core::chat::MentionRef;
use mello_core::Command;
use slint::{ComponentHandle, Model};

use crate::app_context::AppContext;
use crate::converters::apply_unread_to_crews;
use crate::{EmojiItemData, MentionMemberData};

const EMOJIS: &[&str] = &[
    "😀", "😃", "😄", "😁", "😅", "😂", "🤣", "😊", "😇", "🙂", "😉", "😌", "😍", "🥰", "😘", "😋",
    "😛", "😜", "🤪", "😝", "🤑", "🤗", "🤭", "🤫", "🤔", "🤐", "🤨", "😐", "😑", "😶", "😏", "😒",
    "🙄", "😬", "🤥", "😌", "😔", "😪", "🤤", "😴", "😷", "🤒", "🤕", "🤢", "🤮", "👍", "👎", "👊",
    "✊", "🤛", "🤜", "🤞", "✌️", "🤟", "🤘", "👌", "🤌", "🤏", "👈", "👉", "❤️", "🧡", "💛", "💚",
    "💙", "💜", "🖤", "🤍", "🤎", "💔", "❣️", "💕", "💞", "💓", "💗", "🔥", "⭐", "🌟", "✨", "💫",
    "🎉", "🎊", "🎈", "🎁", "🏆", "🥇", "🥈", "🥉", "⚽", "🏀", "🎮", "🕹️", "🎯", "🎲", "🧩", "♟️",
    "🎭", "🎨", "🎬", "🎤", "🎧", "🎼", "🎹", "🥁", "🎷",
];

pub fn default_emoji_list() -> Vec<EmojiItemData> {
    EMOJIS
        .iter()
        .map(|e| EmojiItemData {
            emoji: (*e).into(),
            name: slint::SharedString::default(),
        })
        .collect()
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
    ctx.app
        .set_emoji_list(Rc::new(slint::VecModel::from(default_emoji_list())).into());
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
            move |text| {
                let _ = cmd.send(Command::SendMessage {
                    mentions: take_picks(&picks, &text),
                    content: text.to_string(),
                    reply_to: None,
                });
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
        ctx.app.on_send_message_with_reply(move |text, reply_to| {
            let _ = cmd.send(Command::SendMessage {
                mentions: take_picks(&picks, &text),
                content: text.to_string(),
                reply_to: Some(reply_to.to_string()),
            });
            scroll_ctx.fire();
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_request_history(move || {
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
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_delete_message(move |message_id| {
            let _ = cmd.send(Command::DeleteMessage {
                message_id: message_id.to_string(),
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
        ctx.app.on_gif_hovered(move |preview_url| {
            anim.resume(preview_url.as_str());
        });
    }
    {
        let scroll = ctx.chat_scroll.clone();
        let unread = ctx.unread_tracker.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_chat_viewport_changed(move |at_bottom| {
            scroll.on_viewport_at_bottom(at_bottom);
            if at_bottom {
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
            if let Err(e) = open::that(url.as_str()) {
                log::warn!("Failed to open URL {}: {}", url, e);
            }
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
