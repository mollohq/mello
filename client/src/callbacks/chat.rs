use mello_core::Command;
use slint::ComponentHandle;

use crate::app_context::AppContext;

pub fn wire(ctx: &AppContext) {
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_send_message(move |text| {
            let _ = cmd.try_send(Command::SendMessage {
                content: text.to_string(),
                reply_to: None,
            });
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_send_message_with_reply(move |text, reply_to| {
            let _ = cmd.try_send(Command::SendMessage {
                content: text.to_string(),
                reply_to: Some(reply_to.to_string()),
            });
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        let app_weak = ctx.app.as_weak();
        ctx.app.on_request_history(move || {
            if let Some(a) = app_weak.upgrade() {
                a.set_loading_history(true);
            }
            let _ = cmd.try_send(Command::LoadHistory { cursor: None });
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_edit_message(move |message_id, new_body| {
            let _ = cmd.try_send(Command::EditMessage {
                message_id: message_id.to_string(),
                new_body: new_body.to_string(),
            });
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_delete_message(move |message_id| {
            let _ = cmd.try_send(Command::DeleteMessage {
                message_id: message_id.to_string(),
            });
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_gif_pill_clicked(move || {
            log::info!("[gif] pill clicked, loading trending");
            let _ = cmd.try_send(Command::LoadTrendingGifs);
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
        ctx.app.on_gif_search(move |query| {
            let q = query.to_string();
            if q.is_empty() {
                let _ = cmd.try_send(Command::LoadTrendingGifs);
            } else {
                let _ = cmd.try_send(Command::SearchGifs { query: q });
            }
        });
    }
    {
        let cmd = ctx.cmd_tx.clone();
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
                let _ = cmd.try_send(Command::SendGif {
                    gif,
                    body: String::new(),
                });
            });
    }
    {
        let anim = ctx.gif_chat_anim.clone();
        ctx.app.on_gif_hovered(move |preview_url| {
            anim.resume(preview_url.as_str());
        });
    }
    {
        ctx.app.on_emoji_pill_clicked(|| {
            log::info!("Emoji pill clicked (popover TODO)");
        });
    }
}
