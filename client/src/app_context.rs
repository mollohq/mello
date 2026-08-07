use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;

use mello_core::Command;
use slint::ComponentHandle;

use crate::chat_ui::ChatScrollState;
use crate::foreground_monitor::ForegroundMonitor;
use crate::gif_animator::GifAnimator;
use crate::hud_manager::HudManager;
use crate::snapshot_loader::SnapshotLoader;
use crate::updater::Updater;
use crate::{MainWindow, Settings};
use mello_core::chat::UnreadTracker;

/// (user_id, display_name, is_friend)
pub type InvitedUserList = Vec<(String, String, bool)>;

/// Shared state threaded through all callback and handler modules.
/// Created once in `run()`, passed by reference everywhere.
///
/// Every field is a handle (`Rc`/`Arc`/`Weak`/`Handle`) or a Slint component,
/// so cloning is cheap and shares the same underlying state — it is not a deep
/// copy. The `Clone` impl lives directly below the struct so that adding a
/// field is caught here, rather than silently diverging from a hand-written
/// copy in another module.
pub struct AppContext {
    pub app: MainWindow,
    pub cmd_tx: UnboundedSender<Command>,
    pub settings: Rc<RefCell<Settings>>,
    pub rt: tokio::runtime::Handle,
    pub active_voice_channel: Rc<RefCell<String>>,
    pub new_crew_avatar_b64: Arc<Mutex<Option<String>>>,
    pub crew_settings_avatar_b64: Arc<Mutex<Option<String>>>,
    pub invited_users: Rc<RefCell<InvitedUserList>>,
    pub discover_cursor: Rc<RefCell<Option<String>>>,
    pub discover_loading: Rc<RefCell<bool>>,
    pub chat_messages: Rc<RefCell<Vec<mello_core::events::ChatMessage>>>,
    pub chat_scroll: Rc<ChatScrollState>,
    pub unread_tracker: Rc<RefCell<UnreadTracker>>,
    pub active_crew_id: Rc<RefCell<String>>,
    pub avatar_state: Arc<Mutex<crate::avatar::AvatarGridState>>,
    pub profile_avatar_state: Arc<Mutex<crate::avatar::AvatarGridState>>,
    pub avatar_shuffle_timer: Rc<RefCell<Option<slint::Timer>>>,
    /// Single-shot safety timer that auto-stops a diagnostic capture after a
    /// max duration so a user can't leave verbose logging on indefinitely.
    pub diag_autostop_timer: Rc<RefCell<Option<slint::Timer>>>,
    /// Single-shot timer that auto-dismisses the post-game prompt after 30 s
    /// without interaction (spec 17 §7.2). Cancelled when the user interacts.
    pub post_game_timer: Rc<RefCell<Option<slint::Timer>>>,
    /// Set when a Riot-linkable game session just ended; the next RiotStatus
    /// event decides whether to surface the post-game "connect" CTA.
    pub riot_cta_pending: Rc<Cell<bool>>,
    /// Games settings rows as last received from core; merged with the
    /// disabled set from Settings when pushed to the UI.
    pub games_integrations: Rc<RefCell<Vec<mello_core::events::GameIntegrationStatus>>>,
    /// Unknown-game candidate currently shown in the "track it?" prompt
    /// (exe, path, display name). Consumed on track/dismiss.
    pub pending_unknown_game: Rc<RefCell<Option<(String, String, String)>>>,
    pub muted_before_deafen: Rc<Cell<bool>>,
    pub updater: Rc<RefCell<Option<Updater>>>,
    pub hotkey_mgr: Rc<RefCell<crate::platform::hotkeys::HotkeyManager>>,
    pub status_item: Rc<RefCell<crate::platform::StatusItem>>,
    pub gif_popover_anim: GifAnimator,
    pub gif_chat_anim: GifAnimator,
    pub dbg_hist: Rc<RefCell<crate::DebugHistory>>,
    pub avatar_cache: Rc<RefCell<HashMap<String, slint::Image>>>,
    /// Runtime game icons (exe-extracted or crew-shared), keyed by game_id.
    /// Populated from the disk cache on demand; misses are negative-cached
    /// per run by the fetch glue.
    pub game_icon_cache: Rc<RefCell<HashMap<String, slint::Image>>>,
    pub hud_manager: Rc<HudManager>,
    pub fg_monitor: Rc<RefCell<ForegroundMonitor>>,
    pub pending_deep_link: Rc<RefCell<Option<crate::deep_link::DeepLink>>>,
    pub ipc_listener: Rc<RefCell<Option<crate::ipc::IpcListener>>>,
    pub snapshot_loader: Rc<SnapshotLoader>,
    pub stream_frame_timer: Rc<crate::stream_frame_timer::StreamFrameTimer>,
    #[cfg(target_os = "windows")]
    pub native_frame_slot: mello_core::NativeFrameSlot,
    #[cfg(target_os = "windows")]
    pub frame_lifecycle: mello_core::FrameLifecycleSlot,
    #[cfg(target_os = "windows")]
    pub dcomp_presenter: Rc<RefCell<Option<crate::dcomp_presenter::DCompPresenter>>>,
    #[cfg(target_os = "windows")]
    pub taskbar_toolbar: Rc<RefCell<Option<crate::platform::taskbar_toolbar::TaskbarToolbar>>>,
}

#[cfg(any(test, feature = "testkit"))]
impl AppContext {
    /// Build an `AppContext` that touches no OS resources.
    ///
    /// The caller owns the pieces a test needs to observe or drive:
    /// - `app`: the `MainWindow` to assert against,
    /// - `cmd_tx`: paired with a receiver the test drains to see what the UI
    ///   asked core to do,
    /// - `rt`: a runtime handle, since some components spawn onto it.
    ///
    /// Everything else is either an empty default or an explicitly disabled
    /// variant (no tray icon, no global hotkey listener, no HUD overlay
    /// thread), so this is safe to construct on a headless CI runner.
    pub fn for_test(
        app: crate::MainWindow,
        cmd_tx: UnboundedSender<Command>,
        rt: tokio::runtime::Handle,
    ) -> Self {
        use slint::ComponentHandle as _;

        let frame_consumed = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let frame_lifecycle: mello_core::FrameLifecycleSlot =
            Arc::new(std::sync::atomic::AtomicU8::new(0));
        #[cfg(not(target_os = "windows"))]
        let frame_slot: mello_core::FrameSlot = Arc::new(Mutex::new(None));
        #[cfg(target_os = "windows")]
        let native_frame_slot = mello_core::NativeFrameSlot::default();
        #[cfg(target_os = "windows")]
        let dcomp_presenter: Rc<RefCell<Option<crate::dcomp_presenter::DCompPresenter>>> =
            Rc::new(RefCell::new(None));

        let stream_frame_timer = Rc::new(crate::stream_frame_timer::StreamFrameTimer::new(
            app.as_weak(),
            #[cfg(not(target_os = "windows"))]
            frame_slot,
            frame_consumed,
            frame_lifecycle.clone(),
            #[cfg(target_os = "windows")]
            native_frame_slot.clone(),
            #[cfg(target_os = "windows")]
            dcomp_presenter.clone(),
        ));

        Self {
            app,
            cmd_tx,
            settings: Rc::new(RefCell::new(Settings::default())),
            rt: rt.clone(),
            active_voice_channel: Rc::new(RefCell::new(String::new())),
            new_crew_avatar_b64: Arc::new(Mutex::new(None)),
            crew_settings_avatar_b64: Arc::new(Mutex::new(None)),
            invited_users: Rc::new(RefCell::new(Vec::new())),
            discover_cursor: Rc::new(RefCell::new(None)),
            discover_loading: Rc::new(RefCell::new(false)),
            chat_messages: Rc::new(RefCell::new(Vec::new())),
            chat_scroll: Rc::new(ChatScrollState::new()),
            unread_tracker: Rc::new(RefCell::new(UnreadTracker::new())),
            active_crew_id: Rc::new(RefCell::new(String::new())),
            avatar_state: Arc::new(Mutex::new(crate::avatar::AvatarGridState::new())),
            profile_avatar_state: Arc::new(Mutex::new(crate::avatar::AvatarGridState::new())),
            avatar_shuffle_timer: Rc::new(RefCell::new(None)),
            diag_autostop_timer: Rc::new(RefCell::new(None)),
            post_game_timer: Rc::new(RefCell::new(None)),
            riot_cta_pending: Rc::new(Cell::new(false)),
            games_integrations: Rc::new(RefCell::new(Vec::new())),
            pending_unknown_game: Rc::new(RefCell::new(None)),
            muted_before_deafen: Rc::new(Cell::new(false)),
            updater: Rc::new(RefCell::new(None)),
            hotkey_mgr: Rc::new(RefCell::new(
                crate::platform::hotkeys::HotkeyManager::disabled(),
            )),
            status_item: Rc::new(RefCell::new(crate::platform::StatusItem::disabled())),
            gif_popover_anim: GifAnimator::new(50, None),
            gif_chat_anim: GifAnimator::new(50, Some(2)),
            dbg_hist: Rc::new(RefCell::new(crate::DebugHistory::new())),
            avatar_cache: Rc::new(RefCell::new(HashMap::new())),
            game_icon_cache: Rc::new(RefCell::new(HashMap::new())),
            hud_manager: Rc::new(HudManager::start(false)),
            fg_monitor: Rc::new(RefCell::new(ForegroundMonitor::new(false))),
            pending_deep_link: Rc::new(RefCell::new(None)),
            ipc_listener: Rc::new(RefCell::new(None)),
            snapshot_loader: Rc::new(SnapshotLoader::new(rt)),
            stream_frame_timer,
            #[cfg(target_os = "windows")]
            native_frame_slot,
            #[cfg(target_os = "windows")]
            frame_lifecycle,
            #[cfg(target_os = "windows")]
            dcomp_presenter,
            #[cfg(target_os = "windows")]
            taskbar_toolbar: Rc::new(RefCell::new(None)),
        }
    }
}

impl Clone for AppContext {
    fn clone(&self) -> Self {
        // Hand-written rather than derived: `MainWindow` is a Slint component,
        // which exposes `clone_strong()` instead of implementing `Clone`.
        Self {
            app: self.app.clone_strong(),
            cmd_tx: self.cmd_tx.clone(),
            settings: self.settings.clone(),
            rt: self.rt.clone(),
            active_voice_channel: self.active_voice_channel.clone(),
            new_crew_avatar_b64: self.new_crew_avatar_b64.clone(),
            crew_settings_avatar_b64: self.crew_settings_avatar_b64.clone(),
            invited_users: self.invited_users.clone(),
            discover_cursor: self.discover_cursor.clone(),
            discover_loading: self.discover_loading.clone(),
            chat_messages: self.chat_messages.clone(),
            chat_scroll: self.chat_scroll.clone(),
            unread_tracker: self.unread_tracker.clone(),
            active_crew_id: self.active_crew_id.clone(),
            avatar_state: self.avatar_state.clone(),
            profile_avatar_state: self.profile_avatar_state.clone(),
            avatar_shuffle_timer: self.avatar_shuffle_timer.clone(),
            diag_autostop_timer: self.diag_autostop_timer.clone(),
            post_game_timer: self.post_game_timer.clone(),
            riot_cta_pending: self.riot_cta_pending.clone(),
            games_integrations: self.games_integrations.clone(),
            pending_unknown_game: self.pending_unknown_game.clone(),
            muted_before_deafen: self.muted_before_deafen.clone(),
            updater: self.updater.clone(),
            hotkey_mgr: self.hotkey_mgr.clone(),
            status_item: self.status_item.clone(),
            gif_popover_anim: self.gif_popover_anim.clone(),
            gif_chat_anim: self.gif_chat_anim.clone(),
            dbg_hist: self.dbg_hist.clone(),
            avatar_cache: self.avatar_cache.clone(),
            game_icon_cache: self.game_icon_cache.clone(),
            hud_manager: self.hud_manager.clone(),
            fg_monitor: self.fg_monitor.clone(),
            pending_deep_link: self.pending_deep_link.clone(),
            ipc_listener: self.ipc_listener.clone(),
            snapshot_loader: self.snapshot_loader.clone(),
            stream_frame_timer: self.stream_frame_timer.clone(),
            #[cfg(target_os = "windows")]
            native_frame_slot: self.native_frame_slot.clone(),
            #[cfg(target_os = "windows")]
            frame_lifecycle: self.frame_lifecycle.clone(),
            #[cfg(target_os = "windows")]
            dcomp_presenter: self.dcomp_presenter.clone(),
            #[cfg(target_os = "windows")]
            taskbar_toolbar: self.taskbar_toolbar.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of `for_test`: it must construct on a machine with no
    /// tray, no accessibility permission and no display. If this ever starts
    /// touching a real OS resource it will hang or panic on a CI runner, so
    /// exercising it here keeps that honest.
    #[test]
    fn for_test_builds_without_os_resources() {
        crate::testkit::init_test_backend();

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let app = crate::MainWindow::new().expect("window");
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();

        let ctx = AppContext::for_test(app, cmd_tx, rt.handle().clone());

        // Disabled subsystems must be inert rather than merely present.
        assert!(!ctx.hud_manager.is_enabled(), "HUD overlay must stay off");
        assert!(
            ctx.hotkey_mgr.borrow().poll().is_none(),
            "disabled hotkey manager must never yield events"
        );
        assert!(ctx.updater.borrow().is_none(), "no updater under test");

        // Cloning is what poll_loop does every tick; it must share state, so a
        // write through the clone is visible through the original.
        let cloned = ctx.clone();
        cloned.settings.borrow_mut().onboarding_step = 3;
        assert_eq!(ctx.settings.borrow().onboarding_step, 3);

        drop(cmd_rx);
    }
}
