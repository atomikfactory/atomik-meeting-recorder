//! Where the atomik-kb MCP token lives. The token NEVER touches SQLite, the
//! settings file, or logs -- it only ever goes through the same
//! `CredentialStore` trait AppFlowy's session uses, whose real implementation
//! is the OS credential store (Windows Credential Manager / macOS Keychain /
//! Linux Secret Service via the `keyring` crate), here under its own service
//! name so the two integrations can never read or delete each other's
//! entries. Tests inject `appflowy_credentials::FakeCredentialStore`.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use crate::appflowy_credentials::CredentialStore;
use crate::appflowy_credentials::SharedCredentialStore;

/// The one entry this integration stores. A constant so a typo cannot
/// silently create a second, orphaned credential entry.
pub const KEY_TOKEN: &str = "mcp_token";

/// Distinct from the AppFlowy service name (`com.local.meetingrecorder.appflowy`).
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
const SERVICE: &str = "com.local.meetingrecorder.atomikkb";

/// The real OS-backed store. Only "Clear token" deletes anything, and only
/// its own entry under `SERVICE`.
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

/// atomik-kb tokens are `kbm_` + 43 base64url characters (32 random bytes).
/// Checked before saving so a pasted config snippet or a truncated token is
/// refused up front; the message never echoes what was pasted.
pub fn validate_token_shape(token: &str) -> Result<(), String> {
    let body = token.strip_prefix("kbm_").unwrap_or("");
    let ok = body.len() == 43
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err("That does not look like an atomik-kb token (expected kbm_ followed by 43 characters). Copy it again from atomik-kb: Settings → MCP.".into())
    }
}

pub fn save_token(credentials: &SharedCredentialStore, token: &str) -> Result<(), String> {
    let token = token.trim();
    validate_token_shape(token)?;
    credentials.set(KEY_TOKEN, token)
}
pub fn has_token(credentials: &SharedCredentialStore) -> Result<bool, String> {
    Ok(credentials.get(KEY_TOKEN)?.is_some())
}
pub fn clear_token(credentials: &SharedCredentialStore) -> Result<(), String> {
    credentials.delete(KEY_TOKEN)
}

/// Obviously fake, but the right shape (kbm_ + 43 base64url characters), so
/// tests never need -- and must never contain -- a real token.
#[cfg(test)]
pub const FAKE_TOKEN: &str = "kbm_TEST_FAKE_TOKEN_aaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appflowy_credentials::FakeCredentialStore;
    use std::sync::Arc;

    #[test]
    fn the_fake_token_has_the_real_shape() {
        assert_eq!(FAKE_TOKEN.len(), 47);
        assert!(validate_token_shape(FAKE_TOKEN).is_ok());
    }
    #[test]
    fn malformed_tokens_are_refused_without_echoing_them() {
        for bad in [
            "",
            "kbm_",
            "kbm_short",
            "abc_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "kbm_has space aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            let error = validate_token_shape(bad).unwrap_err();
            assert!(bad.len() <= 4 || !error.contains(bad), "{error}");
        }
    }
    #[test]
    fn token_round_trips_trimmed_and_clears_idempotently() {
        let store: SharedCredentialStore = Arc::new(FakeCredentialStore::new());
        assert!(!has_token(&store).unwrap());
        save_token(&store, &format!("  {FAKE_TOKEN}\n")).unwrap();
        assert!(has_token(&store).unwrap());
        assert_eq!(store.get(KEY_TOKEN).unwrap().as_deref(), Some(FAKE_TOKEN));
        clear_token(&store).unwrap();
        clear_token(&store).unwrap();
        assert!(!has_token(&store).unwrap());
    }
    #[test]
    fn an_invalid_token_is_never_stored() {
        let store: SharedCredentialStore = Arc::new(FakeCredentialStore::new());
        assert!(save_token(&store, "kbm_nope").is_err());
        assert!(!has_token(&store).unwrap());
    }
}
