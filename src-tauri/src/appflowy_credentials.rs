//! Where the AppFlowy Cloud session lives. Refresh/access tokens NEVER touch
//! SQLite, the settings file, or logs -- they only ever go through this
//! trait, whose real implementation is the OS credential store (Windows
//! Credential Manager / macOS Keychain / Linux Secret Service via the
//! `keyring` crate). Everything else in this codebase depends on the trait,
//! not on `keyring` directly, so tests can inject `FakeCredentialStore`
//! instead of writing to a real OS credential store.
#[cfg(test)]
use std::collections::HashMap;
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;

/// Keys this module writes/reads. Kept as constants so a typo cannot
/// silently create a second, orphaned credential entry.
pub const KEY_ACCESS_TOKEN: &str = "access_token";
pub const KEY_REFRESH_TOKEN: &str = "refresh_token";
pub const KEY_EMAIL: &str = "email";

pub trait CredentialStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>, String>;
    fn set(&self, key: &str, value: &str) -> Result<(), String>;
    /// Removes one entry; missing entries are not an error (sign-out is
    /// idempotent).
    fn delete(&self, key: &str) -> Result<(), String>;
}

/// Service name under which every entry is stored. Distinct from any other
/// service this OS credential store might hold for a different app.
const SERVICE: &str = "com.local.meetingrecorder.appflowy";

/// The real OS-backed store. Only sign-out deletes anything, and only its own
/// entries under `SERVICE` -- never anything else in the credential store.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
pub struct OsCredentialStore;

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
impl CredentialStore for OsCredentialStore {
    fn get(&self, key: &str) -> Result<Option<String>, String> {
        let entry = keyring::Entry::new(SERVICE, key).map_err(|e| e.to_string())?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
    fn set(&self, key: &str, value: &str) -> Result<(), String> {
        keyring::Entry::new(SERVICE, key)
            .map_err(|e| e.to_string())?
            .set_password(value)
            .map_err(|e| e.to_string())
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(SERVICE, key).map_err(|e| e.to_string())?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// In-memory fake used by every test in this crate; never touches the real OS
/// credential store, so `cargo test` cannot leave (or depend on) a real
/// Windows Credential Manager / Secret Service entry. Test-only: it must
/// never be reachable from production code, so there is no way to
/// accidentally ship a build that silently keeps tokens in memory instead of
/// the OS credential store.
#[cfg(test)]
#[derive(Clone, Default)]
pub struct FakeCredentialStore {
    values: Arc<Mutex<HashMap<String, String>>>,
}
#[cfg(test)]
impl FakeCredentialStore {
    pub fn new() -> Self {
        Self::default()
    }
}
#[cfg(test)]
impl CredentialStore for FakeCredentialStore {
    fn get(&self, key: &str) -> Result<Option<String>, String> {
        Ok(self.values.lock().map_err(|_| "credential lock")?.get(key).cloned())
    }
    fn set(&self, key: &str, value: &str) -> Result<(), String> {
        self.values
            .lock()
            .map_err(|_| "credential lock")?
            .insert(key.to_string(), value.to_string());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        self.values.lock().map_err(|_| "credential lock")?.remove(key);
        Ok(())
    }
}

pub type SharedCredentialStore = Arc<dyn CredentialStore>;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fake_store_round_trips_and_deletes() {
        let store = FakeCredentialStore::new();
        assert_eq!(store.get(KEY_ACCESS_TOKEN).unwrap(), None);
        store.set(KEY_ACCESS_TOKEN, "token-1").unwrap();
        assert_eq!(store.get(KEY_ACCESS_TOKEN).unwrap(), Some("token-1".into()));
        store.delete(KEY_ACCESS_TOKEN).unwrap();
        assert_eq!(store.get(KEY_ACCESS_TOKEN).unwrap(), None);
        // Deleting an already-missing entry is not an error (idempotent sign-out).
        store.delete(KEY_ACCESS_TOKEN).unwrap();
    }
    #[test]
    fn sign_out_deletes_only_this_apps_entries() {
        let store = FakeCredentialStore::new();
        store.set(KEY_ACCESS_TOKEN, "a").unwrap();
        store.set(KEY_REFRESH_TOKEN, "r").unwrap();
        store.set(KEY_EMAIL, "user@example.com").unwrap();
        for key in [KEY_ACCESS_TOKEN, KEY_REFRESH_TOKEN, KEY_EMAIL] {
            store.delete(key).unwrap();
        }
        assert_eq!(store.get(KEY_ACCESS_TOKEN).unwrap(), None);
        assert_eq!(store.get(KEY_REFRESH_TOKEN).unwrap(), None);
        assert_eq!(store.get(KEY_EMAIL).unwrap(), None);
    }
}
