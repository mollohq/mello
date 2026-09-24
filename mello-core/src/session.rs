use keyring::Entry;

const SERVICE: &str = "mello";
const USER: &str = "session";

/// Keyring account name holding the refresh token, namespaced by
/// `MELLO_SESSION_KEY` when set.
///
/// The keyring is a single machine-wide store that `MELLO_CONFIG_DIR` does not
/// cover. Without this the release smoke test — which ends in `delete_account`,
/// and therefore in `session::clear()` — wipes the *developer's* saved session
/// on the build machine every release. The runners double as development
/// machines, so that is a real signout, not a test artifact.
fn account() -> String {
    match std::env::var("MELLO_SESSION_KEY") {
        Ok(suffix) if !suffix.is_empty() => format!("{}.{}", USER, suffix),
        _ => USER.to_string(),
    }
}

/// The session file for e2e runs, when the `e2e-session` feature is on and
/// `MELLO_E2E_SESSION_FILE` is set. Each test user gets its own file.
#[cfg(feature = "e2e-session")]
fn e2e_file() -> Option<std::path::PathBuf> {
    std::env::var_os("MELLO_E2E_SESSION_FILE")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
}

pub fn save(refresh_token: &str) -> Result<(), String> {
    #[cfg(feature = "e2e-session")]
    if let Some(path) = e2e_file() {
        return std::fs::write(path, refresh_token).map_err(|e| e.to_string());
    }
    let entry = Entry::new(SERVICE, &account()).map_err(|e| e.to_string())?;
    entry.set_password(refresh_token).map_err(|e| e.to_string())
}

pub fn load() -> Option<String> {
    #[cfg(feature = "e2e-session")]
    if let Some(path) = e2e_file() {
        return std::fs::read_to_string(path).ok().filter(|t| !t.is_empty());
    }
    let entry = Entry::new(SERVICE, &account()).ok()?;
    entry.get_password().ok()
}

pub fn clear() {
    #[cfg(feature = "e2e-session")]
    if let Some(path) = e2e_file() {
        let _ = std::fs::remove_file(path);
        return;
    }
    if let Ok(entry) = Entry::new(SERVICE, &account()) {
        let _ = entry.delete_credential();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the env mutation below. Tests share one process, so an
    /// unguarded `set_var` leaks into whichever test runs concurrently.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn account_defaults_to_the_shared_entry() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("MELLO_SESSION_KEY");
        assert_eq!(account(), "session");
    }

    #[test]
    fn account_is_namespaced_when_a_key_is_set() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("MELLO_SESSION_KEY", "smoke");
        assert_eq!(account(), "session.smoke");
        std::env::remove_var("MELLO_SESSION_KEY");
    }

    /// An empty value must fall back rather than create a `session.` entry that
    /// silently differs from the default — CI passing an unset variable through
    /// as "" is normal.
    #[test]
    fn empty_key_falls_back_to_the_default() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("MELLO_SESSION_KEY", "");
        assert_eq!(account(), "session");
        std::env::remove_var("MELLO_SESSION_KEY");
    }

    /// With the file set, the token never touches the keyring: a rebuilt test
    /// binary would otherwise block on a keychain prompt.
    #[cfg(feature = "e2e-session")]
    #[test]
    fn e2e_session_file_round_trips() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = std::env::temp_dir().join(format!("mello-e2e-session-{}", std::process::id()));
        std::env::set_var("MELLO_E2E_SESSION_FILE", &path);

        save("rt-123").expect("save to file");
        assert_eq!(
            std::fs::read_to_string(&path).ok().as_deref(),
            Some("rt-123")
        );
        assert_eq!(load().as_deref(), Some("rt-123"));
        clear();
        assert!(!path.exists());
        assert_eq!(load(), None);

        std::env::remove_var("MELLO_E2E_SESSION_FILE");
    }
}
