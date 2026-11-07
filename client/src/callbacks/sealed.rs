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
