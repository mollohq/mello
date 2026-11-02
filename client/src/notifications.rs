use notify_rust::Notification;

/// The AppUserModelID that Velopack puts on the Start menu and desktop
/// shortcuts: `velopack.<packId>`, with `--packId m3llo` in release.yml.
/// Windows shows a toast with the name and icon of the shortcut that has
/// this id. Without it, notify-rust sends the toast as Windows PowerShell.
#[cfg(target_os = "windows")]
const TOAST_APP_ID: &str = "velopack.m3llo";

/// A notification with the app's identity on it.
fn notification() -> Notification {
    #[allow(unused_mut)]
    let mut n = Notification::new();
    #[cfg(target_os = "windows")]
    n.app_id(TOAST_APP_ID);
    n
}

/// Shows an OS notification and returns true when the OS accepted it.
///
/// A seam (`AppContext::tray_notice`): the test harness sets none, so a test
/// never posts a real notification.
pub type ShowNotice = Box<dyn Fn() -> bool>;

pub const TRAY_NOTICE_TITLE: &str = "m3llo is still running";

#[cfg(target_os = "macos")]
pub const TRAY_NOTICE_BODY: &str =
    "It stays in the menu bar to keep collecting your gaming stats. Quit it from the menu bar icon.";
#[cfg(not(target_os = "macos"))]
pub const TRAY_NOTICE_BODY: &str =
    "It stays in the tray to keep collecting your gaming stats. Right-click the tray icon to quit.";

/// Tell the user that the closed window went to the tray, and that m3llo
/// did not quit. Users who did not know this thought the app was closed.
pub fn notify_closed_to_tray() -> bool {
    match notification()
        .summary(TRAY_NOTICE_TITLE)
        .body(TRAY_NOTICE_BODY)
        .show()
    {
        Ok(_) => true,
        Err(e) => {
            log::warn!("[close] tray notice not shown: {e}");
            false
        }
    }
}

pub fn notify_member_joined(name: &str) {
    notification()
        .summary("Crew")
        .body(&format!("{} jumped in", name))
        .timeout(notify_rust::Timeout::Milliseconds(4000))
        .show()
        .ok();
}

pub fn notify_message(crew: &str, sender: &str, preview: &str) {
    notification()
        .summary(crew)
        .body(&format!("{}: {}", sender, preview))
        .timeout(notify_rust::Timeout::Milliseconds(4000))
        .show()
        .ok();
}

#[allow(dead_code)]
pub fn notify_invite(inviter: &str, crew: &str) {
    notification()
        .summary(&format!("{} invite", crate::APP_NAME))
        .body(&format!("{} invited you to {}", inviter, crew))
        .timeout(notify_rust::Timeout::Milliseconds(0)) // persistent
        .show()
        .ok();
}
