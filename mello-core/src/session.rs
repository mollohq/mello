use keyring::Entry;

const SERVICE: &str = "mello";
const USER: &str = "session";

pub fn save(refresh_token: &str) -> Result<(), String> {
    let entry = Entry::new(SERVICE, USER).map_err(|e| e.to_string())?;
    entry.set_password(refresh_token).map_err(|e| e.to_string())
}

pub fn load() -> Option<String> {
    let entry = Entry::new(SERVICE, USER).ok()?;
    entry.get_password().ok()
}

pub fn clear() {
    if let Ok(entry) = Entry::new(SERVICE, USER) {
        let _ = entry.delete_credential();
    }
}
