use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::Sender;

use mello_core::Command;

use crate::foreground_monitor::ForegroundMonitor;
use crate::gif_animator::GifAnimator;
use crate::hud_manager::HudManager;
use crate::updater::Updater;
use crate::{MainWindow, Settings};

/// (user_id, display_name, is_friend)
pub type InvitedUserList = Vec<(String, String, bool)>;

/// Shared state threaded through all callback and handler modules.
/// Created once in main(), passed by reference everywhere.
pub struct AppContext {
    pub app: MainWindow,
    pub cmd_tx: Sender<Command>,
    pub settings: Rc<RefCell<Settings>>,
    pub rt: tokio::runtime::Handle,
    pub active_voice_channel: Rc<RefCell<String>>,
    pub new_crew_avatar_b64: Arc<Mutex<Option<String>>>,
    pub invited_users: Rc<RefCell<InvitedUserList>>,
    pub discover_cursor: Rc<RefCell<Option<String>>>,
    pub discover_loading: Rc<RefCell<bool>>,
    pub chat_messages: Rc<RefCell<Vec<mello_core::events::ChatMessage>>>,
    pub avatar_state: Arc<Mutex<crate::avatar::AvatarGridState>>,
    pub profile_avatar_state: Arc<Mutex<crate::avatar::AvatarGridState>>,
    pub avatar_shuffle_timer: Rc<RefCell<Option<slint::Timer>>>,
    pub muted_before_deafen: Rc<Cell<bool>>,
    pub updater: Rc<RefCell<Option<Updater>>>,
    pub hotkey_mgr: Rc<RefCell<crate::platform::hotkeys::HotkeyManager>>,
    pub status_item: Rc<RefCell<crate::platform::StatusItem>>,
    pub gif_popover_anim: GifAnimator,
    pub gif_chat_anim: GifAnimator,
    pub dbg_hist: Rc<RefCell<crate::DebugHistory>>,
    pub avatar_cache: Rc<RefCell<HashMap<String, slint::Image>>>,
    pub hud_manager: Rc<HudManager>,
    pub fg_monitor: Rc<RefCell<ForegroundMonitor>>,
    pub pending_deep_link: Rc<RefCell<Option<crate::deep_link::DeepLink>>>,
    pub ipc_listener: Rc<RefCell<Option<crate::ipc::IpcListener>>>,
}
