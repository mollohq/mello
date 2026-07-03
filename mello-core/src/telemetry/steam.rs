//! Shared Steam install discovery for GSI-style adapters (Windows-first,
//! mirroring specs 17/18; other platforms return `Unsupported` upstream).

use std::path::PathBuf;

use super::TelemetryError;

/// Steam install root from the registry.
pub(crate) fn steam_root() -> Result<PathBuf, TelemetryError> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let steam_path: String = hkcu
        .open_subkey("Software\\Valve\\Steam")
        .ok()
        .and_then(|k| k.get_value("SteamPath").ok())
        .ok_or_else(|| TelemetryError::GameNotFound("Steam not found in registry".into()))?;
    Ok(PathBuf::from(steam_path))
}

/// All Steam library folders: the install root plus any extra libraries from
/// `libraryfolders.vdf`.
pub(crate) fn library_folders() -> Result<Vec<PathBuf>, TelemetryError> {
    let root = steam_root()?;
    let mut libraries = vec![root.clone()];
    let lib_vdf = root.join("steamapps").join("libraryfolders.vdf");
    if let Ok(contents) = std::fs::read_to_string(&lib_vdf) {
        libraries.extend(parse_library_paths(&contents));
    }
    Ok(libraries)
}

/// Locate `steamapps/common/<app_dir>/<suffix…>` in any Steam library.
pub(crate) fn find_app_subdir(
    app_dir: &str,
    suffix: &[&str],
    not_found: &str,
) -> Result<PathBuf, TelemetryError> {
    for lib in library_folders()? {
        let mut dir = lib.join("steamapps").join("common").join(app_dir);
        for part in suffix {
            dir = dir.join(part);
        }
        if dir.is_dir() {
            return Ok(dir);
        }
    }
    Err(TelemetryError::GameNotFound(not_found.into()))
}

/// Heuristic launch-option check: does any Steam user's `localconfig.vdf`
/// mention `needle`? Read-only — we never edit Steam's config (a wrong write
/// there can corrupt every app's launch options). A false positive (the flag
/// set on another app) only suppresses a setup hint, never breaks telemetry.
pub(crate) fn any_localconfig_contains(needle: &str) -> bool {
    let Ok(root) = steam_root() else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(root.join("userdata")) else {
        return false;
    };
    for entry in entries.flatten() {
        let cfg = entry.path().join("config").join("localconfig.vdf");
        if let Ok(contents) = std::fs::read_to_string(&cfg) {
            if contents.contains(needle) {
                return true;
            }
        }
    }
    false
}

/// Extract `"path"` values from a `libraryfolders.vdf`. Minimal VDF handling:
/// each library object has a `"path"  "<dir>"` line with `\\`-escaped separators.
fn parse_library_paths(vdf: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in vdf.lines() {
        let line = line.trim();
        let lower = line.to_ascii_lowercase();
        if !lower.starts_with("\"path\"") {
            continue;
        }
        // Take the second quoted token on the line.
        let mut parts = line.split('"').filter(|s| !s.trim().is_empty());
        let _key = parts.next(); // "path"
        if let Some(raw) = parts.next() {
            out.push(PathBuf::from(raw.replace("\\\\", "\\")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_library_paths_extracts_dirs() {
        let vdf = r#"
"libraryfolders"
{
    "0"
    {
        "path"    "C:\\Program Files (x86)\\Steam"
        "apps" { "730" "1234" }
    }
    "1"
    {
        "path"    "D:\\SteamLibrary"
    }
}
"#;
        let paths = parse_library_paths(vdf);
        assert_eq!(paths.len(), 2);
        assert_eq!(
            paths[0],
            std::path::PathBuf::from("C:\\Program Files (x86)\\Steam")
        );
        assert_eq!(paths[1], std::path::PathBuf::from("D:\\SteamLibrary"));
    }
}
