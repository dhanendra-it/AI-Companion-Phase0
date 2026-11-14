//! API keys live in Windows Credential Manager, one entry per provider name.
//! New entries use the Aalu name; existing AI Companion entries migrate on read.

const SERVICE: &str = "Aalu";
const LEGACY_SERVICE: &str = "AI Companion";

fn password(service: &str, name: &str) -> Option<String> {
    keyring::Entry::new(service, name).ok()?.get_password().ok()
}

/// Returns a saved key and migrates any legacy credential into the Aalu service.
pub fn get(name: &str) -> Option<String> {
    if let Some(value) = password(SERVICE, name) {
        let value = value.trim().to_string();
        if !value.is_empty() { return Some(value); }
    }
    let value = password(LEGACY_SERVICE, name)?.trim().to_string();
    if value.is_empty() { return None; }
    if let Ok(entry) = keyring::Entry::new(SERVICE, name) { let _ = entry.set_password(&value); }
    Some(value)
}

/// Saves a key. An empty value deletes both current and legacy credentials.
pub fn set(name: &str, value: &str) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        for service in [SERVICE, LEGACY_SERVICE] {
            let entry = keyring::Entry::new(service, name).map_err(|e| e.to_string())?;
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => {},
                Err(error) => return Err(error.to_string()),
            }
        }
        return Ok(());
    }
    let entry = keyring::Entry::new(SERVICE, name).map_err(|e| e.to_string())?;
    entry.set_password(value).map_err(|e| e.to_string())
}
