pub(crate) mod auth;
mod chat;
mod clip;
pub(crate) mod crew;
mod crew_settings;
mod game;
mod games;
pub(crate) mod onboarding;
mod settings;
mod streaming;
mod voice;

use crate::app_context::AppContext;

pub use chat::refresh_mention_members;

pub fn wire_all(ctx: &AppContext) {
    auth::wire(ctx);
    crew::wire(ctx);
    crew_settings::wire(ctx);
    voice::wire(ctx);
    chat::wire(ctx);
    clip::wire(ctx);
    streaming::wire(ctx);
    settings::wire(ctx);
    onboarding::wire(ctx);
    game::wire(ctx);
    games::wire(ctx);
}
