//! Linux desktop integration for the AppImage build.
//!
//! Velopack's AppImage carries a desktop file inside the image, but it does not
//! install it, and it registers no URL scheme. So the app itself writes, into
//! the user's XDG data folder:
//!
//! - `applications/app.m3llo.M3llo.desktop`: the menu entry and the
//!   `mello://` handler (`MimeType=x-scheme-handler/mello`);
//! - `icons/hicolor/512x512/apps/app.m3llo.M3llo.png`: the icon.
//!
//! The file name of the entry is the app ID that the window carries
//! (`LINUX_APP_ID`, set as the Wayland app_id and the X11 WM_CLASS), so the
//! desktop matches the window to its name and icon.
//!
//! Only an AppImage build registers (`$APPIMAGE` is set). A development build
//! run from `target/` leaves the user's menu alone. Velopack replaces the
//! AppImage at the same path on update, so the entry stays valid; it is
//! rewritten when the AppImage moves.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{APP_NAME, LINUX_APP_ID};

const ICON_PNG: &[u8] = include_bytes!("../../assets/icons/mello.png");

/// Register the menu entry, the icon and the `mello://` handler when running
/// from an AppImage. Failures are logged; the app runs without them.
pub fn register() {
    let Some(appimage) = std::env::var_os("APPIMAGE").filter(|p| !p.is_empty()) else {
        log::info!("[desktop] not an AppImage; no menu entry or link handler registered");
        return;
    };
    let Some(data_home) = data_home() else {
        log::warn!("[desktop] no XDG data folder; no menu entry registered");
        return;
    };
    let appimage = PathBuf::from(appimage);

    let apps = data_home.join("applications");
    let entry_path = apps.join(format!("{LINUX_APP_ID}.desktop"));
    let entry = desktop_entry(&appimage);
    let icon_path = data_home
        .join("icons/hicolor/512x512/apps")
        .join(format!("{LINUX_APP_ID}.png"));

    let entry_changed = write_if_changed(&entry_path, entry.as_bytes());
    let icon_changed = write_if_changed(&icon_path, ICON_PNG);

    if entry_changed {
        // All three tools are optional: the desktop picks the entry up without
        // them, only later. KDE resolves links from its own service cache
        // (kde-open); without a rebuild it opened mello:// in the browser for
        // a few seconds after the first start (2026-10-31). The scheme default
        // needs xdg-mime.
        run_tool("update-desktop-database", &[apps.as_os_str()]);
        run_tool("kbuildsycoca6", &[]);
        run_tool(
            "xdg-mime",
            &[
                "default".as_ref(),
                format!("{LINUX_APP_ID}.desktop").as_ref(),
                "x-scheme-handler/mello".as_ref(),
            ],
        );
    }
    log::info!(
        "[desktop] registered {} (entry {}, icon {})",
        entry_path.display(),
        if entry_changed {
            "written"
        } else {
            "unchanged"
        },
        if icon_changed { "written" } else { "unchanged" },
    );
}

/// `$XDG_DATA_HOME`, else `~/.local/share`.
fn data_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".local/share"))
}

/// The desktop entry for the AppImage at `appimage`.
fn desktop_entry(appimage: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={APP_NAME}\n\
         Comment=Hang out with your crew.\n\
         Exec={} %u\n\
         Icon={LINUX_APP_ID}\n\
         Terminal=false\n\
         Categories=Network;Chat;Game;\n\
         MimeType=x-scheme-handler/mello;\n\
         StartupWMClass={LINUX_APP_ID}\n",
        exec_quote(&appimage.to_string_lossy())
    )
}

/// Quote a program path for an `Exec` key (Desktop Entry spec, "The Exec
/// key"): inside double quotes, a backslash goes before `"`, `` ` ``, `$` and
/// `\`. A literal `%` is `%%`. The entry file then escapes each backslash
/// once more, because string values treat `\\` as one backslash.
fn exec_quote(path: &str) -> String {
    let mut quoted = String::with_capacity(path.len() + 2);
    quoted.push('"');
    for c in path.chars() {
        match c {
            '"' | '`' | '$' => {
                quoted.push_str("\\\\");
                quoted.push(c);
            }
            '\\' => quoted.push_str("\\\\\\\\"),
            '%' => quoted.push_str("%%"),
            _ => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// Write `content` to `path` unless it holds exactly that already. True when
/// the file was written.
fn write_if_changed(path: &Path, content: &[u8]) -> bool {
    if std::fs::read(path).is_ok_and(|current| current == content) {
        return false;
    }
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            log::warn!("[desktop] cannot create {}: {e}", parent.display());
            return false;
        }
    }
    match std::fs::write(path, content) {
        Ok(()) => true,
        Err(e) => {
            log::warn!("[desktop] cannot write {}: {e}", path.display());
            false
        }
    }
}

fn run_tool(tool: &str, args: &[&std::ffi::OsStr]) {
    match Command::new(tool).args(args).status() {
        Ok(status) if status.success() => {}
        Ok(status) => log::warn!("[desktop] {tool} exited with {status}"),
        Err(e) => log::info!("[desktop] {tool} not run: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_path_is_quoted() {
        assert_eq!(
            exec_quote("/home/bob/Applications/m3llo.AppImage"),
            "\"/home/bob/Applications/m3llo.AppImage\""
        );
    }

    #[test]
    fn spaces_stay_inside_the_quotes() {
        assert_eq!(
            exec_quote("/home/bob/My Apps/m3llo"),
            "\"/home/bob/My Apps/m3llo\""
        );
    }

    #[test]
    fn reserved_characters_are_escaped_for_the_entry_file() {
        // `$` -> `\$` for Exec, then `\\$` in the file.
        assert_eq!(exec_quote("/a$b"), "\"/a\\\\$b\"");
        assert_eq!(exec_quote("/a\"b"), "\"/a\\\\\"b\"");
        assert_eq!(exec_quote("/a`b"), "\"/a\\\\`b\"");
        // `\` -> `\\` for Exec, then `\\\\` in the file.
        assert_eq!(exec_quote("/a\\b"), "\"/a\\\\\\\\b\"");
        assert_eq!(exec_quote("/100%/m3llo"), "\"/100%%/m3llo\"");
    }

    #[test]
    fn the_entry_names_the_app_id_and_the_link_scheme() {
        let entry = desktop_entry(Path::new("/opt/m3llo.AppImage"));
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("Name=m3llo\n"));
        assert!(entry.contains("Exec=\"/opt/m3llo.AppImage\" %u\n"));
        assert!(entry.contains("Icon=app.m3llo.M3llo\n"));
        assert!(entry.contains("MimeType=x-scheme-handler/mello;\n"));
        assert!(entry.contains("StartupWMClass=app.m3llo.M3llo\n"));
    }

    #[test]
    fn an_unchanged_file_is_not_written_again() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("sub/entry.desktop");
        assert!(write_if_changed(&path, b"one"));
        assert!(!write_if_changed(&path, b"one"));
        assert!(write_if_changed(&path, b"two"));
        assert_eq!(std::fs::read(&path).expect("read back"), b"two");
    }
}
