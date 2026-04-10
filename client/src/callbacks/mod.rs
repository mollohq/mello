mod auth;
mod chat;
mod clip;
mod crew;
mod game;
mod onboarding;
mod settings;
mod streaming;
mod voice;

use crate::app_context::AppContext;

pub fn wire_all(ctx: &AppContext) {
    auth::wire(ctx);
    crew::wire(ctx);
    voice::wire(ctx);
    chat::wire(ctx);
    clip::wire(ctx);
    streaming::wire(ctx);
    settings::wire(ctx);
    onboarding::wire(ctx);
    game::wire(ctx);
}
