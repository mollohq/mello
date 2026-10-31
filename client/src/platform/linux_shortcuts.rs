//! Push-to-talk on a Wayland session through the GlobalShortcuts portal.
//!
//! Wayland gives no app the keys of another app's window, so `rdev` sees
//! nothing while a game has focus. The portal (KDE since Plasma 5.27, GNOME
//! since 48) delivers `Activated` and `Deactivated` for a shortcut that the
//! user confirms once in the desktop's dialog.
//!
//! The thread binds nothing until push-to-talk has a key, so a user who never
//! uses push-to-talk never sees the dialog. A new key binds again in a new
//! session. The key is a preferred trigger only: the user may choose another
//! one in the dialog or in the desktop's shortcut settings.
//!
//! Known KDE issue (bug 484525, Plasma 6.7): releasing another key while the
//! push-to-talk key is held can send an early `Deactivated`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
use ashpd::desktop::Session;
use futures_util::StreamExt;

use super::hotkeys::{PttBinding, PttEvent};
use crate::LINUX_APP_ID;

const PTT_ID: &str = "push-to-talk";

/// True on a Wayland session, where the portal is the only way to see a key
/// while another app has focus.
pub fn wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty())
}

/// Start the portal thread. Errors only when the thread cannot start; a
/// missing portal is logged on the thread and push-to-talk stays off.
pub fn start(
    binding: Arc<Mutex<Option<PttBinding>>>,
    active: Arc<AtomicBool>,
    event_tx: mpsc::Sender<PttEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    std::thread::Builder::new()
        .name("ptt-portal".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    log::error!("[hotkeys] portal runtime failed: {e}");
                    return;
                }
            };
            runtime.block_on(run(binding, active, event_tx));
            log::warn!("[hotkeys] portal thread exited");
        })?;
    Ok(())
}

async fn run(
    binding: Arc<Mutex<Option<PttBinding>>>,
    active: Arc<AtomicBool>,
    event_tx: mpsc::Sender<PttEvent>,
) {
    // An app outside a sandbox names itself to the portal, so the desktop
    // files the shortcut under m3llo. xdg-desktop-portal before 1.19 has no
    // Registry; the portal then uses what it can find.
    match ashpd::AppID::try_from(LINUX_APP_ID) {
        Ok(app_id) => {
            if let Err(e) = ashpd::register_host_app(app_id).await {
                log::info!("[hotkeys] portal app registration skipped: {e}");
            }
        }
        Err(e) => log::warn!("[hotkeys] invalid app id {LINUX_APP_ID}: {e}"),
    }

    let portal = match GlobalShortcuts::new().await {
        Ok(p) => p,
        Err(e) => {
            log::warn!("[hotkeys] no GlobalShortcuts portal, push-to-talk off: {e}");
            return;
        }
    };
    let (mut activated, mut deactivated) = match (
        portal.receive_activated().await,
        portal.receive_deactivated().await,
    ) {
        (Ok(a), Ok(d)) => (a, d),
        (Err(e), _) | (_, Err(e)) => {
            log::warn!("[hotkeys] portal signals unavailable: {e}");
            return;
        }
    };
    log::info!("[hotkeys] GlobalShortcuts portal ready");

    let mut session: Option<Session<'_, GlobalShortcuts<'_>>> = None;
    // The binding the current session was made for, even when the user
    // cancelled the dialog: a cancelled key does not open the dialog again.
    let mut bound_for: Option<PttKey> = None;
    let mut held = false;
    let mut tick = tokio::time::interval(Duration::from_millis(500));

    loop {
        tokio::select! {
            Some(event) = activated.next() => {
                if event.shortcut_id() == PTT_ID && session.is_some()
                    && active.load(Ordering::Relaxed) && !held
                {
                    held = true;
                    log::info!("[hotkeys] PTT pressed (portal)");
                    let _ = event_tx.send(PttEvent::Pressed);
                }
            }
            Some(event) = deactivated.next() => {
                if event.shortcut_id() == PTT_ID && held {
                    held = false;
                    log::info!("[hotkeys] PTT released (portal)");
                    let _ = event_tx.send(PttEvent::Released);
                }
            }
            _ = tick.tick() => {
                if held && !active.load(Ordering::Relaxed) {
                    held = false;
                    let _ = event_tx.send(PttEvent::Released);
                }
                let wanted = binding.lock().ok().and_then(|b| b.as_ref().map(PttKey::of));
                if wanted == bound_for {
                    continue;
                }
                if let Some(old) = session.take() {
                    let _ = old.close().await;
                }
                bound_for = wanted.clone();
                if let Some(key) = wanted {
                    session = bind(&portal, &key).await;
                }
            }
        }
    }
}

/// One session with the push-to-talk shortcut, after the user confirmed it.
async fn bind<'a>(
    portal: &GlobalShortcuts<'a>,
    key: &PttKey,
) -> Option<Session<'a, GlobalShortcuts<'a>>> {
    let session = match portal.create_session().await {
        Ok(s) => s,
        Err(e) => {
            log::warn!("[hotkeys] portal session failed: {e}");
            return None;
        }
    };
    let shortcut =
        NewShortcut::new(PTT_ID, "Push to talk").preferred_trigger(key.trigger.as_deref());
    let response = match portal.bind_shortcuts(&session, &[shortcut], None).await {
        Ok(request) => request.response(),
        Err(e) => {
            log::warn!("[hotkeys] portal bind failed: {e}");
            return None;
        }
    };
    match response {
        Ok(bound) => {
            for s in bound.shortcuts() {
                log::info!(
                    "[hotkeys] portal bound {} to {}",
                    s.id(),
                    s.trigger_description()
                );
            }
            Some(session)
        }
        Err(e) => {
            log::warn!("[hotkeys] push-to-talk shortcut not bound (dialog cancelled?): {e}");
            None
        }
    }
}

/// The parts of a binding that decide the shortcut.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PttKey {
    code: String,
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
    trigger: Option<String>,
}

impl PttKey {
    fn of(b: &PttBinding) -> Self {
        Self {
            code: b.code_str.clone(),
            ctrl: b.ctrl,
            alt: b.alt,
            shift: b.shift,
            meta: b.meta,
            trigger: xdg_trigger(b),
        }
    }
}

/// The binding as an XDG "shortcuts" trigger, for example `CTRL+SHIFT+F9`:
/// modifiers CTRL, ALT, SHIFT and LOGO, then an XKB key name. None for a key
/// without a mapping here; the user then picks the key in the dialog.
fn xdg_trigger(b: &PttBinding) -> Option<String> {
    let key = xkb_key_name(&b.code_str)?;
    let mut parts: Vec<&str> = Vec::new();
    if b.ctrl {
        parts.push("CTRL");
    }
    if b.alt {
        parts.push("ALT");
    }
    if b.shift {
        parts.push("SHIFT");
    }
    if b.meta {
        parts.push("LOGO");
    }
    parts.push(&key);
    Some(parts.join("+"))
}

/// KeyboardEvent.code names (the settings store these) to XKB key names.
fn xkb_key_name(code: &str) -> Option<String> {
    if let Some(letter) = code.strip_prefix("Key") {
        if letter.len() == 1 && letter.chars().all(|c| c.is_ascii_uppercase()) {
            return Some(letter.to_ascii_lowercase());
        }
    }
    if let Some(digit) = code.strip_prefix("Digit") {
        if digit.len() == 1 && digit.chars().all(|c| c.is_ascii_digit()) {
            return Some(digit.to_string());
        }
    }
    if let Some(n) = code.strip_prefix('F') {
        if n.parse::<u8>().is_ok_and(|n| (1..=24).contains(&n)) {
            return Some(code.to_string());
        }
    }
    let name = match code {
        "Space" => "space",
        "Tab" => "Tab",
        "CapsLock" => "Caps_Lock",
        "Backquote" => "grave",
        "Minus" => "minus",
        "Equal" => "equal",
        "BracketLeft" => "bracketleft",
        "BracketRight" => "bracketright",
        "Backslash" => "backslash",
        "Semicolon" => "semicolon",
        "Quote" => "apostrophe",
        "Comma" => "comma",
        "Period" => "period",
        "Slash" => "slash",
        "Insert" => "Insert",
        "Delete" => "Delete",
        "Home" => "Home",
        "End" => "End",
        "PageUp" => "Prior",
        "PageDown" => "Next",
        "ArrowUp" => "Up",
        "ArrowDown" => "Down",
        "ArrowLeft" => "Left",
        "ArrowRight" => "Right",
        _ => return None,
    };
    Some(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(code: &str, ctrl: bool, alt: bool, shift: bool, meta: bool) -> PttBinding {
        PttBinding {
            code_str: code.into(),
            ctrl,
            alt,
            shift,
            meta,
        }
    }

    #[test]
    fn letters_digits_and_function_keys_map_to_xkb_names() {
        assert_eq!(
            xdg_trigger(&binding("KeyV", false, false, false, false)).as_deref(),
            Some("v")
        );
        assert_eq!(
            xdg_trigger(&binding("Digit4", false, false, false, false)).as_deref(),
            Some("4")
        );
        assert_eq!(
            xdg_trigger(&binding("F9", false, false, false, false)).as_deref(),
            Some("F9")
        );
        assert_eq!(
            xdg_trigger(&binding("Backquote", false, false, false, false)).as_deref(),
            Some("grave")
        );
    }

    #[test]
    fn modifiers_come_first_in_a_fixed_order() {
        assert_eq!(
            xdg_trigger(&binding("F9", true, true, true, true)).as_deref(),
            Some("CTRL+ALT+SHIFT+LOGO+F9")
        );
        assert_eq!(
            xdg_trigger(&binding("KeyT", false, false, true, false)).as_deref(),
            Some("SHIFT+t")
        );
    }

    #[test]
    fn an_unmapped_key_has_no_preferred_trigger() {
        assert_eq!(
            xdg_trigger(&binding("MediaPlayPause", false, false, false, false)),
            None
        );
        assert_eq!(
            xdg_trigger(&binding("F25", false, false, false, false)),
            None
        );
        assert_eq!(
            xdg_trigger(&binding("KeyAB", false, false, false, false)),
            None
        );
    }

    #[test]
    fn a_new_key_is_a_different_binding() {
        let a = PttKey::of(&binding("F9", false, false, false, false));
        let b = PttKey::of(&binding("F10", false, false, false, false));
        assert_ne!(a, b);
        assert_eq!(a, PttKey::of(&binding("F9", false, false, false, false)));
    }
}
