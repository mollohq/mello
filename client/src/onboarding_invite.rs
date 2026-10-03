//! An invite link on a fresh install (#68).
//!
//! A fresh install opened from `mello://join/<code>` skips step 1. The client
//! resolves the code before an account exists (core uses the `http_key`), and
//! opens the welcome screen: who invited the user, to which crew. "Join" opens
//! step 2 with the invited crew shown. Finalize then joins that crew by its
//! invite code, so a private crew works too. "Not now" opens step 1 and
//! forgets the invite.
//!
//! An invite that cannot be used lands on step 1 with a message. Step 1 always
//! offers a crew or "create your own", so the user is never stuck.
//!
//! A user with a device account, or a logged-in user, keeps the join modal:
//! the pending link is sent after sign-in (`handlers::auth`).
//!
//! The web lounge copies the invite link to the clipboard when the user
//! downloads the app. On the first launch of a fresh install with no deep
//! link, startup reads the clipboard once. A join link on the lounge host
//! takes the path of a deep link. Any other text is ignored: it is not
//! logged, stored or sent, and the clipboard is not changed.
//!
//! The clipboard can also hold something else. Step 1 has the
//! invite-code card for that: the user pastes the link or the code, and the
//! resolve takes the same path as a deep link (CREW-INVITES §8.5). The card
//! also works for a user with a device account who logged out: that user is
//! on step 1, and finalize joins the crew into the existing account.

use std::cell::RefCell;
use std::rc::Rc;

use mello_core::crew::{InviteError, ResolvedInvite};
use mello_core::Command;
use tokio::sync::mpsc::UnboundedSender;

use crate::app_context::AppContext;
use crate::deep_link::DeepLink;
use crate::handlers::{invite_join_error_message, invite_resolve_error_message, InviteSource};
use crate::onboarding::{EffectCtx, Input, OnboardingState};

/// Does an invite go into onboarding, instead of the join modal?
///
/// For a fresh install that has not created its account: no session, no
/// device account, and a step before the account exists.
///
/// Also for an invite that the user typed into the card on step 1, with a
/// device account: the user logged out, has no session, and cannot join in
/// the modal. Finalize joins the crew into the device account instead.
pub fn opens_onboarding(app: &crate::MainWindow, settings: &crate::Settings) -> bool {
    opens_onboarding_at(
        OnboardingState::from_step(app.get_onboarding_step()),
        app.get_logged_in(),
        settings.has_device_account(),
        app.get_onboarding_invite_code_checking(),
    )
}

fn opens_onboarding_at(
    state: OnboardingState,
    logged_in: bool,
    has_device_account: bool,
    typed_in_card: bool,
) -> bool {
    let before_account = matches!(
        state,
        OnboardingState::Loading
            | OnboardingState::PickCrew
            | OnboardingState::InviteWelcome
            | OnboardingState::PickAvatar
    );
    !logged_in && (!has_device_account || typed_in_card) && before_account
}

/// At startup, resolve an invite link now when it goes into onboarding.
///
/// `state` is the step that startup resumes. Call before `onboarding::resume`:
/// core runs commands in order, so the resolve answers before crew discovery,
/// and step 1 does not show first. Any other link stays pending until the
/// user signs in.
pub fn dispatch_at_startup(ctx: &AppContext, state: OnboardingState) {
    // Taken on every start, so the clipboard is read once at most.
    let clipboard = ctx.startup_clipboard.borrow_mut().take();
    let has_device_account = ctx.settings.borrow().has_device_account();
    if !opens_onboarding_at(state, false, has_device_account, false) {
        return;
    }
    show_pending(ctx);

    // Only on `Loading`, the first launch. A later launch is on step 1 or
    // further, and a link the user declined there must not open again.
    if state == OnboardingState::Loading && ctx.pending_deep_link.borrow().is_none() {
        if let Some(code) = clipboard.and_then(StartupClipboard::invite_code) {
            log::info!("[invite] fresh install: the clipboard holds a lounge invite");
            *ctx.pending_deep_link.borrow_mut() = Some(DeepLink::Join { code });
        }
    }

    let is_join = matches!(*ctx.pending_deep_link.borrow(), Some(DeepLink::Join { .. }));
    if !is_join {
        return;
    }
    if let Some(DeepLink::Join { code }) = ctx.pending_deep_link.borrow_mut().take() {
        log::info!("[invite] fresh install opened from an invite — resolving {code} before step 2");
        let _ = ctx.cmd_tx.send(Command::ResolveCrewInvite { code });
    }
}

/// A file that replaces the system clipboard in an `e2e` build.
#[cfg(feature = "e2e")]
pub const E2E_CLIPBOARD_ENV: &str = "MELLO_E2E_CLIPBOARD_FILE";

/// The clipboard, read at most once by [`dispatch_at_startup`].
///
/// A reader instead of the text: startup reads the clipboard only for a
/// fresh install with no deep link. Tests give a reader that does not touch
/// the system clipboard.
pub struct StartupClipboard {
    /// The lounge host from the build config. Empty: any host.
    pub lounge_host: String,
    pub read: Box<dyn FnOnce() -> Option<String>>,
}

impl StartupClipboard {
    /// The system clipboard, through `arboard`.
    ///
    /// An `e2e` build reads the file in `MELLO_E2E_CLIPBOARD_FILE` instead,
    /// when it is set. The driver gives each journey one file: the clipboard
    /// of its machine. No journey reads the clipboard of the developer.
    pub fn system(lounge_host: String) -> Self {
        #[cfg(feature = "e2e")]
        if let Some(path) = std::env::var_os(E2E_CLIPBOARD_ENV) {
            log::info!("[e2e] the clipboard is the file {}", path.to_string_lossy());
            return Self {
                lounge_host,
                read: Box::new(move || std::fs::read_to_string(path).ok()),
            };
        }
        Self {
            lounge_host,
            read: Box::new(|| arboard::Clipboard::new().ok()?.get_text().ok()),
        }
    }

    /// The invite code, when the clipboard holds a lounge join link.
    ///
    /// The text is dropped here. Text that is not a join link is not logged.
    fn invite_code(self) -> Option<String> {
        let text = (self.read)()?;
        crate::deep_link::lounge_link_code(&text, &self.lounge_host)
    }
}

/// The step that startup resumes, given the persisted one.
///
/// The welcome screen shows the stored invite. With no stored invite it has
/// nothing to show, so startup opens step 1 instead of an empty screen.
pub fn resume_state(settings: &crate::Settings, state: OnboardingState) -> OnboardingState {
    let stored = settings.pending_invite_code.is_some() && settings.pending_invite.is_some();
    if state == OnboardingState::InviteWelcome && !stored {
        log::warn!("[invite] welcome screen persisted with no stored invite — resuming step 1");
        return OnboardingState::PickCrew;
    }
    state
}

/// Show an invite that a previous run stored, for a restart on step 2.
///
/// The avatar is not stored: load it again.
fn show_pending(ctx: &AppContext) {
    let name = ctx.settings.borrow().pending_invite_crew_name.clone();
    if let Some(name) = name.filter(|_| ctx.settings.borrow().pending_invite_code.is_some()) {
        set_crew(&ctx.app, &name);
        fetch_avatar(&ctx.cmd_tx, &ctx.settings);
    }
}

/// Ask core for the avatar of the stored invite's crew.
///
/// The resolve answer has no avatar. `get_crew_avatar` needs no session:
/// core uses the `http_key`, as step 1 does for the crews it lists. The
/// answer arrives as `CrewAvatarLoaded` (`handlers::crew`).
fn fetch_avatar(cmd_tx: &UnboundedSender<Command>, settings: &Rc<RefCell<crate::Settings>>) {
    let crew_id = settings
        .borrow()
        .pending_invite
        .as_ref()
        .map(|invite| invite.crew_id.clone());
    if let Some(crew_id) = crew_id.filter(|id| !id.is_empty()) {
        let _ = cmd_tx.send(Command::FetchCrewAvatars {
            crew_ids: vec![crew_id],
        });
    }
}

/// The crew avatar arrived. Show it when it belongs to the stored invite.
pub fn avatar_loaded(
    app: &crate::MainWindow,
    settings: &Rc<RefCell<crate::Settings>>,
    crew_id: &str,
    image: &slint::Image,
) {
    let is_invited_crew = settings
        .borrow()
        .pending_invite
        .as_ref()
        .is_some_and(|invite| invite.crew_id == crew_id);
    if is_invited_crew {
        app.set_onboarding_invite_crew_avatar(image.clone());
        app.set_onboarding_invite_crew_has_avatar(true);
    }
}

/// The invite resolved: store it and open the welcome screen.
///
/// Stores the whole invite, not only the code: a restart on the welcome
/// screen shows it again from Settings. Resolving again would need the
/// network, and a failure there would leave the screen with nothing to show.
pub fn accept(ctx: &AppContext, code: String, invite: ResolvedInvite) {
    log::info!(
        "[invite] onboarding invite crew={:?} id={} inviter={:?} — opening the welcome screen",
        invite.crew_name,
        invite.crew_id,
        invite.inviter.as_ref().map(|p| p.display_name.as_str()),
    );
    {
        let mut s = ctx.settings.borrow_mut();
        s.pending_invite_code = Some(code);
        s.pending_invite_crew_name = Some(invite.crew_name.clone());
        s.pending_invite = Some(invite);
        s.onboarding_via_invite = true;
        // One crew only: the invite replaces a crew picked before.
        s.pending_crew_id = None;
        s.pending_crew_name = None;
        s.save();
    }
    // Also when the welcome screen is already open: a second link replaces
    // the first, and the entry effect runs only on a change of state.
    show_stored(&ctx.app, &ctx.settings);
    ctx.app.set_onboarding_invite_error("".into());
    // The card that sent this resolve is done. The typed text stays until
    // "Not now" clears it.
    ctx.app.set_onboarding_invite_code_checking(false);
    ctx.app.set_onboarding_invite_code_error("".into());
    // A second invite replaces the first, and the avatar with it.
    ctx.app
        .set_onboarding_invite_crew_avatar(Default::default());
    ctx.app.set_onboarding_invite_crew_has_avatar(false);
    fetch_avatar(&ctx.cmd_tx, &ctx.settings);
    crate::onboarding::advance(ctx, Input::InviteResolved);
}

/// "Open invite" on step 1, or Enter in the field: what the user typed.
///
/// A text that is no invite gets the message in the card and no network
/// call. A valid one sends `ResolveCrewInvite`, and the answer takes the
/// path of a deep link (`accept`, `resolve_failed`). A second press while the
/// resolve runs does nothing.
pub fn open_typed(app: &crate::MainWindow, cmd_tx: &UnboundedSender<Command>, text: &str) {
    if app.get_onboarding_invite_code_checking() {
        log::debug!("[invite] card: a resolve is running — ignoring");
        return;
    }
    match crate::deep_link::parse_invite_input(text) {
        Some(code) => {
            log::info!("[invite] card: resolving {code}");
            app.set_onboarding_invite_code_error("".into());
            app.set_onboarding_invite_code_checking(true);
            let _ = cmd_tx.send(Command::ResolveCrewInvite { code });
        }
        None => {
            log::info!("[invite] card: the text is not an invite — no network call");
            app.set_onboarding_invite_code_error(
                invite_resolve_error_message(InviteError::InvalidCode, InviteSource::TypedCode)
                    .into(),
            );
        }
    }
}

/// The user edited the field in the card: the message no longer applies.
pub fn code_edited(app: &crate::MainWindow) {
    app.set_onboarding_invite_code_error("".into());
}

/// "Back" on step 2 of the invite path: the welcome screen, with the invite
/// kept. The welcome screen shows it again from `Settings`.
pub fn back(app: &crate::MainWindow, settings: &Rc<RefCell<crate::Settings>>, fx: &EffectCtx) {
    log::info!("[invite] step 2: back — opening the welcome screen, the invite is kept");
    crate::onboarding::advance_with(
        app,
        settings,
        fx,
        Input::GoBackTo(OnboardingState::InviteWelcome),
    );
}

/// "Join" on the welcome screen: step 2, which keeps the invite.
pub fn join(app: &crate::MainWindow, settings: &Rc<RefCell<crate::Settings>>, fx: &EffectCtx) {
    log::info!("[invite] welcome screen: join — opening step 2");
    crate::onboarding::advance_with(app, settings, fx, Input::InviteAccepted);
}

/// "Not now" on the welcome screen: step 1, and the invite is forgotten.
/// Finalize then joins or creates the crew that the user picks there.
pub fn decline(app: &crate::MainWindow, settings: &Rc<RefCell<crate::Settings>>, fx: &EffectCtx) {
    log::info!("[invite] welcome screen: not now — forgetting the invite, opening step 1");
    clear(app, settings);
    crate::onboarding::advance_with(app, settings, fx, Input::InviteDeclined);
}

/// Put the stored invite on the welcome screen and on step 2.
///
/// The entry effect of the welcome screen, so every way in shows the same
/// data, a restart included.
pub fn show_stored(app: &crate::MainWindow, settings: &Rc<RefCell<crate::Settings>>) {
    let invite = settings.borrow().pending_invite.clone();
    match invite {
        Some(invite) => show(app, &invite),
        None => log::warn!("[invite] welcome screen with no stored invite"),
    }
}

/// The invite did not resolve: step 1 says why. Discovery moves `Loading` on
/// to step 1, so this does not change the step.
///
/// An invite that the user typed shows the message in the card, with the
/// field marked. A link that opened the app shows it above the crews.
pub fn resolve_failed(ctx: &AppContext, error: InviteError) {
    if ctx.app.get_onboarding_invite_code_checking() {
        log::warn!("[invite] card: the invite did not resolve: {error:?} — message in the card");
        ctx.app.set_onboarding_invite_code_checking(false);
        ctx.app.set_onboarding_invite_code_error(
            invite_resolve_error_message(error, InviteSource::TypedCode).into(),
        );
        return;
    }
    log::warn!("[invite] onboarding invite did not resolve: {error:?} — step 1 with a message");
    ctx.app.set_onboarding_invite_error(
        invite_resolve_error_message(error, InviteSource::Link).into(),
    );
}

/// Finalize could not join the invited crew. The account exists.
///
/// A failure that a retry can fix stays on step 2 with the message. A code
/// that no longer works, a full crew or a refusal goes back to step 1: the
/// user picks another crew there.
pub fn join_failed(ctx: &AppContext, error: InviteError) {
    ctx.app.set_onboarding_busy(false);
    let message = invite_join_error_message(error, InviteSource::Link);
    if error == InviteError::Failed {
        log::warn!("[invite] finalize could not join the invited crew — retry on step 2");
        ctx.app.set_link_error(message.into());
        return;
    }
    log::warn!("[invite] finalize cannot join the invited crew: {error:?} — back to step 1");
    clear(&ctx.app, &ctx.settings);
    ctx.app.set_onboarding_invite_error(message.into());
    crate::onboarding::advance(ctx, Input::GoBackTo(OnboardingState::PickCrew));
}

/// Forget the invite: the user chose another crew, or onboarding finished.
///
/// Clears what the user typed in the card too. It does not end a resolve in
/// flight: only the answer does.
///
/// Does not save the settings. Every caller moves onboarding next, and the
/// single writer in `onboarding` saves them.
pub fn clear(app: &crate::MainWindow, settings: &Rc<RefCell<crate::Settings>>) {
    {
        let mut s = settings.borrow_mut();
        s.pending_invite_code = None;
        s.pending_invite_crew_name = None;
        s.pending_invite = None;
    }
    set_crew(app, "");
    app.set_onboarding_invite_crew_avatar(Default::default());
    app.set_onboarding_invite_crew_has_avatar(false);
    app.set_onboarding_invite_code_text("".into());
    app.set_onboarding_invite_code_error("".into());
    app.set_onboarding_invite_highlight("".into());
    app.set_onboarding_invite_member_count(0);
    app.set_onboarding_invite_members(Rc::new(slint::VecModel::default()).into());
    app.set_onboarding_invite_inviter(Default::default());
}

fn set_crew(app: &crate::MainWindow, name: &str) {
    app.set_onboarding_invite_crew_name(name.into());
    app.set_onboarding_invite_crew_initials(crate::converters::make_initials(name).into());
}

fn show(app: &crate::MainWindow, invite: &ResolvedInvite) {
    set_crew(app, &invite.crew_name);
    app.set_onboarding_invite_highlight(invite.highlight.as_str().into());
    app.set_onboarding_invite_member_count(invite.member_count);
    let members: Vec<_> = invite
        .members
        .iter()
        .map(crate::converters::invite_person)
        .collect();
    app.set_onboarding_invite_members(Rc::new(slint::VecModel::from(members)).into());
    app.set_onboarding_invite_inviter(
        invite
            .inviter
            .as_ref()
            .map(crate::converters::invite_person)
            .unwrap_or_default(),
    );
}
