//! Window state on KDE Plasma through a KWin script (spec 17 §2.9).
//!
//! Wayland gives no app the windows of another. KWin's scripting API does:
//! each scan loads a short script that lists the normal windows (pid,
//! caption, fullscreen, active), the script calls back on the session bus,
//! and the script is unloaded again. The result fills the window fields that
//! libmello leaves empty on Linux, so the primary game follows focus and a
//! provisional game needs a window, as on Windows.
//!
//! Proton games are X11 windows under Xwayland; KWin reports their pid, which
//! is the Wine process of the game executable that libmello lists.
//!
//! Off when KWin is not on the bus (GNOME, other desktops): the window fields
//! then stay empty, and a session still ends when its process exits.

use std::path::PathBuf;
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::Duration;

use serde::Deserialize;

use crate::game_sensing::RawGameProcess;

const BUS_NAME: &str = "app.m3llo.M3llo.Windows";
const OBJECT_PATH: &str = "/Windows";
const PLUGIN: &str = "m3llo-windows";
const REPLY_TIMEOUT: Duration = Duration::from_secs(1);

const SCRIPT: &str = r#"(function () {
    var out = [];
    var active = workspace.activeWindow;
    var list = workspace.windowList();
    for (var i = 0; i < list.length; i++) {
        var w = list[i];
        if (!w.normalWindow || !w.pid) continue;
        out.push({ pid: w.pid, caption: String(w.caption), fullscreen: !!w.fullScreen, active: w === active });
    }
    callDBus("app.m3llo.M3llo.Windows", "/Windows", "app.m3llo.M3llo.Windows", "Report", JSON.stringify(out));
})();
"#;

/// One window as the script reports it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct KwinWindow {
    pub pid: u32,
    pub caption: String,
    pub fullscreen: bool,
    pub active: bool,
}

struct Receiver {
    tx: mpsc::Sender<String>,
}

#[zbus::interface(name = "app.m3llo.M3llo.Windows")]
impl Receiver {
    fn report(&self, json: String) {
        let _ = self.tx.send(json);
    }
}

struct Bridge {
    conn: zbus::blocking::Connection,
    rx: mpsc::Receiver<String>,
    script: PathBuf,
}

impl Bridge {
    fn connect() -> Option<Self> {
        let (tx, rx) = mpsc::channel();
        let conn = match zbus::blocking::connection::Builder::session()
            .and_then(|b| b.name(BUS_NAME))
            .and_then(|b| b.serve_at(OBJECT_PATH, Receiver { tx }))
            .and_then(|b| b.build())
        {
            Ok(c) => c,
            Err(e) => {
                log::info!("[kwin] no session bus or name taken, window state off: {e}");
                return None;
            }
        };
        let kwin_present = zbus::blocking::fdo::DBusProxy::new(&conn)
            .ok()
            .and_then(|p| {
                let name = zbus::names::BusName::try_from("org.kde.KWin").ok()?;
                p.name_has_owner(name).ok()
            })
            .unwrap_or(false);
        if !kwin_present {
            log::info!("[kwin] KWin is not running, window state off");
            return None;
        }
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let script = dir.join("m3llo-kwin-windows.js");
        if let Err(e) = std::fs::write(&script, SCRIPT) {
            log::warn!("[kwin] cannot write {}: {e}", script.display());
            return None;
        }
        log::info!("[kwin] window state from KWin");
        Some(Self { conn, rx, script })
    }

    fn scripting(&self) -> zbus::Result<zbus::blocking::Proxy<'_>> {
        zbus::blocking::Proxy::new(
            &self.conn,
            "org.kde.KWin",
            "/Scripting",
            "org.kde.kwin.Scripting",
        )
    }

    /// The windows now, or None when KWin did not answer in time.
    fn query(&self) -> Option<Vec<KwinWindow>> {
        let scripting = self.scripting().ok()?;
        // A script left loaded by an earlier crash would block the name.
        let _: zbus::Result<bool> = scripting.call("unloadScript", &(PLUGIN,));
        while self.rx.try_recv().is_ok() {}

        let path = self.script.to_string_lossy().to_string();
        let id: i32 = match scripting.call("loadScript", &(path, PLUGIN)) {
            Ok(id) if id >= 0 => id,
            Ok(_) | Err(_) => {
                log::debug!("[kwin] loadScript failed");
                return None;
            }
        };
        // KWin 6 serves a loaded script at /Scripting/Script<id>, KWin 5 at /<id>.
        let ran = [format!("/Scripting/Script{id}"), format!("/{id}")]
            .iter()
            .any(|object| {
                zbus::blocking::Proxy::new(
                    &self.conn,
                    "org.kde.KWin",
                    object.as_str(),
                    "org.kde.kwin.Script",
                )
                .and_then(|p| p.call::<_, _, ()>("run", &()))
                .is_ok()
            });
        let reply = if ran {
            self.rx.recv_timeout(REPLY_TIMEOUT).ok()
        } else {
            None
        };
        let _: zbus::Result<bool> = scripting.call("unloadScript", &(PLUGIN,));
        serde_json::from_str(&reply?).ok()
    }
}

/// Fill the window fields of `processes` from KWin. No change when KWin is
/// not running or does not answer.
pub(crate) fn annotate(processes: &mut [RawGameProcess]) {
    static BRIDGE: OnceLock<Option<Mutex<Bridge>>> = OnceLock::new();
    let Some(bridge) = BRIDGE.get_or_init(|| Bridge::connect().map(Mutex::new)) else {
        return;
    };
    let Ok(bridge) = bridge.lock() else {
        return;
    };
    if let Some(windows) = bridge.query() {
        static FIRST: std::sync::Once = std::sync::Once::new();
        FIRST.call_once(|| log::info!("[kwin] first window list: {} windows", windows.len()));
        apply(processes, &windows);
    }
}

/// Give each process the state of its windows: the title of its active
/// window, else of its fullscreen window, else of its first titled window;
/// fullscreen and foreground when any of its windows is.
pub(crate) fn apply(processes: &mut [RawGameProcess], windows: &[KwinWindow]) {
    for p in processes.iter_mut() {
        let mine: Vec<&KwinWindow> = windows.iter().filter(|w| w.pid == p.pid).collect();
        if mine.is_empty() {
            continue;
        }
        let titled = |w: &&&KwinWindow| !w.caption.trim().is_empty();
        let title = mine
            .iter()
            .filter(titled)
            .find(|w| w.active)
            .or_else(|| mine.iter().filter(titled).find(|w| w.fullscreen))
            .or_else(|| mine.iter().find(titled));
        if let Some(w) = title {
            p.window_title = w.caption.clone();
        }
        p.is_fullscreen = mine.iter().any(|w| w.fullscreen);
        p.is_foreground = mine.iter().any(|w| w.active);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32) -> RawGameProcess {
        RawGameProcess {
            pid,
            name: "cs2.exe".into(),
            exe: "cs2.exe".into(),
            is_fullscreen: false,
            path: "/home/u/cs2.exe".into(),
            window_title: String::new(),
            is_foreground: false,
            started_at_ms: 0,
        }
    }

    fn window(pid: u32, caption: &str, fullscreen: bool, active: bool) -> KwinWindow {
        KwinWindow {
            pid,
            caption: caption.into(),
            fullscreen,
            active,
        }
    }

    #[test]
    fn a_process_takes_the_state_of_its_window() {
        let mut ps = vec![process(10), process(11)];
        apply(&mut ps, &[window(10, "Counter-Strike 2", true, true)]);
        assert_eq!(ps[0].window_title, "Counter-Strike 2");
        assert!(ps[0].is_fullscreen && ps[0].is_foreground);
        assert!(ps[1].window_title.is_empty() && !ps[1].is_fullscreen && !ps[1].is_foreground);
    }

    #[test]
    fn the_active_window_names_a_process_with_several() {
        let mut ps = vec![process(10)];
        apply(
            &mut ps,
            &[
                window(10, "Launcher", false, false),
                window(10, "Game", false, true),
            ],
        );
        assert_eq!(ps[0].window_title, "Game");
        assert!(ps[0].is_foreground);
    }

    #[test]
    fn an_untitled_window_gives_no_title() {
        let mut ps = vec![process(10)];
        apply(&mut ps, &[window(10, "  ", true, false)]);
        assert!(ps[0].window_title.is_empty());
        assert!(ps[0].is_fullscreen);
    }

    #[test]
    fn the_script_reply_parses() {
        let json = r#"[{"pid":4321,"caption":"Hades II","fullscreen":true,"active":false}]"#;
        let windows: Vec<KwinWindow> = serde_json::from_str(json).expect("parse");
        assert_eq!(windows, vec![window(4321, "Hades II", true, false)]);
    }
}
