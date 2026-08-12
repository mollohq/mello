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

pub fn save(refresh_token: &str) -> Result<(), String> {
    let entry = Entry::new(SERVICE, &account()).map_err(|e| e.to_string())?;
    entry.set_password(refresh_token).map_err(|e| e.to_string())
}

pub fn load() -> Option<String> {
    let entry = Entry::new(SERVICE, &account()).ok()?;
    entry.get_password().ok()
}

pub fn clear() {
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
}
