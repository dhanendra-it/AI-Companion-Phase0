//! API keys live in Windows Credential Manager, one entry per provider name.
//! They are never written to config.json, logs or the chat window.

const SERVICE: &str = "AI Companion";

/// Returns the saved key for a provider, or `None` if there is none.
pub fn get(name: &str) -> Option<String> {
    let entry = keyring::Entry::new(SERVICE, name).ok()?;
    entry.get_password().ok().map(|k| k.trim().to_string()).filter(|k| !k.is_empty())
}

/// Saves a key. An empty value deletes the saved key.
pub fn set(name: &str, value: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(SERVICE, name).map_err(|e| e.to_string())?;
    let value = value.trim();
    if value.is_empty() {
        return match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.to_string()),
        };
    }
    entry.set_password(value).map_err(|e| e.to_string())
}
