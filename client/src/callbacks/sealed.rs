//! The centre view of a sealed card (`sealed.rs`, spec `SEALED-REVEAL.md`).

use slint::ComponentHandle;

use crate::app_context::AppContext;
use crate::{head2head, sealed};

pub fn wire(ctx: &AppContext) {
    let ui = ctx.app.global::<crate::SealedUi>();

    // A card leaves its slot: store its key. Every slot of the belt opens
    // when the card lands, and the cards are built again then.
    {
        let settings = ctx.settings.clone();
        let app_weak = ctx.app.as_weak();
        ui.on_opened(move |key| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let crew = app.get_active_crew_id().to_string();
            if crew.is_empty() || key.is_empty() {
                return;
            }
            let mut s = settings.borrow_mut();
            if sealed::mark_opened(&mut s, &crew, key.as_str(), head2head::now_ms()) {
                log::info!("Sealed card opened: {key}");
                s.save();
            }
        });
    }

    // The owner opened a sealed quest gift. Store its key and build the
    // zones again; the tile shows the gift. The centre view of the reveal
    // comes in front of this with the gift faces (crew quests phase 4).
    {
        let ctx = ctx.clone();
        ctx.app.clone_strong().on_quest_gift_opened(move |key| {
            let crew = ctx.app.get_active_crew_id().to_string();
            if crew.is_empty() || !key.starts_with("quest:") {
                return;
            }
            {
                let mut s = ctx.settings.borrow_mut();
                if !sealed::mark_opened(&mut s, &crew, key.as_str(), head2head::now_ms()) {
                    return;
                }
                s.save();
            }
            log::info!("Quest gift opened: {key}");
            crate::handlers::rebuild_sealed_cards(&ctx);
        });
    }

    // The card is back in its slot: build the cards again, then clear the
    // centre state. The other order would show the slot sealed for a frame.
    {
        let ctx = ctx.clone();
        ui.on_landed(move || {
            crate::handlers::rebuild_sealed_cards(&ctx);
            ctx.app.global::<crate::SealedUi>().invoke_clear();
        });
    }
}
