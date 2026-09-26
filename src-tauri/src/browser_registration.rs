//! Registers/unregisters this app's native-messaging host with installed
//! browsers, from an explicit "Connect browser extension" / "Disconnect"
//! Settings action -- never at startup. Every path this module writes to is
//! *derived deterministically* from the browser and the home directory, so
//! "Disconnect" can recompute and remove exactly what "Connect" created
//! without needing to remember anything separately.
//!
//! Filesystem and (Windows) registry access go through small traits so the
//! test suite exercises the real install/uninstall logic against fakes --
//! no test ever touches a real registry key or a real browser's profile
//! directory.

use serde_json::json;
use std::path::{Path, PathBuf};

pub const HOST_NAME: &str = "com.local.meetingrecorder.browser_bridge";
/// Pinned Chromium extension ID (see browser-extension/key/, derived from
/// the committed public key in browser-extension/manifest.json).
pub const CHROMIUM_EXTENSION_ID: &str = "difnhplcifmjpdhmlfplchdaobhcpocc";
pub const FIREFOX_EXTENSION_ID: &str = "{e9fd5ec2-42a5-4212-8ed9-d2d3a1c43e43}";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Browser {
    Chrome,
    Edge,
    Firefox,
}
impl Browser {
    pub const ALL: [Browser; 3] = [Browser::Chrome, Browser::Edge, Browser::Firefox];
    pub fn label(self) -> &'static str {
        match self {
            Browser::Chrome => "Chrome",
            Browser::Edge => "Edge",
            Browser::Firefox => "Firefox",
        }
    }
}

pub trait FileWriter {
    fn create_dir_all(&mut self, path: &Path) -> Result<(), String>;
    fn write(&mut self, path: &Path, contents: &[u8]) -> Result<(), String>;
    /// Must not error if the path is already absent (idempotent remove).
    fn remove(&mut self, path: &Path) -> Result<(), String>;
    fn exists(&self, path: &Path) -> bool;
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub trait RegistryWriter {
    /// Sets `subkey`'s default (unnamed) value to `manifest_path`, creating
    /// `subkey` under `HKEY_CURRENT_USER` if needed.
    fn set_default_value(&mut self, subkey: &str, manifest_path: &str) -> Result<(), String>;
    /// Deletes `subkey` if present; must not error if it is already absent.
    fn delete_key(&mut self, subkey: &str) -> Result<(), String>;
}

/// One browser's manifest file location, and (Windows only) the registry
/// subkey that must point to it.
fn manifest_file_path(home: &Path, browser: Browser) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        // On Windows the registry value is authoritative; the file can live
        // anywhere stable, but each browser needs its own file since the
        // schema differs (allowed_origins vs. allowed_extensions).
        let slug = match browser {
            Browser::Chrome => "chrome",
            Browser::Edge => "edge",
            Browser::Firefox => "firefox",
        };
        home.join("AppData")
            .join("Local")
            .join("MeetingRecorder")
            .join("native-messaging-hosts")
            .join(format!("{HOST_NAME}.{slug}.json"))
    }
    #[cfg(target_os = "macos")]
    {
        let dir = match browser {
            Browser::Chrome => "Google/Chrome",
            Browser::Edge => "Microsoft Edge",
            Browser::Firefox => "Mozilla",
        };
        home.join("Library/Application Support")
            .join(dir)
            .join("NativeMessagingHosts")
            .join(format!("{HOST_NAME}.json"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let dir = match browser {
            Browser::Chrome => ".config/google-chrome",
            Browser::Edge => ".config/microsoft-edge",
            Browser::Firefox => ".mozilla/native-messaging-hosts",
        };
        let suffix = match browser {
            Browser::Firefox => PathBuf::new(),
            _ => PathBuf::from("NativeMessagingHosts"),
        };
        home.join(dir).join(suffix).join(format!("{HOST_NAME}.json"))
    }
}

#[cfg(target_os = "windows")]
fn registry_subkey(browser: Browser) -> String {
    let vendor = match browser {
        Browser::Chrome => r"Software\Google\Chrome\NativeMessagingHosts",
        Browser::Edge => r"Software\Microsoft\Edge\NativeMessagingHosts",
        Browser::Firefox => r"Software\Mozilla\NativeMessagingHosts",
    };
    format!(r"{vendor}\{HOST_NAME}")
}

fn manifest_json(browser: Browser, exe_path: &Path) -> Vec<u8> {
    let exe = exe_path.display().to_string();
    let body = match browser {
        Browser::Firefox => json!({
            "name": HOST_NAME,
            "description": "Atomik Meeting Recorder browser-detection bridge",
            "path": exe,
            "type": "stdio",
            "allowed_extensions": [FIREFOX_EXTENSION_ID],
        }),
        Browser::Chrome | Browser::Edge => json!({
            "name": HOST_NAME,
            "description": "Atomik Meeting Recorder browser-detection bridge",
            "path": exe,
            "type": "stdio",
            "allowed_origins": [format!("chrome-extension://{CHROMIUM_EXTENSION_ID}/")],
        }),
    };
    serde_json::to_vec_pretty(&body).unwrap_or_default()
}

/// Writes a native-messaging host manifest for every browser in
/// `Browser::ALL`, plus the Windows registry pointer for each, using the
/// exe at `exe_path` as the host executable (invoked later with
/// `--native-messaging-host`). Returns the manifest file paths written, for
/// display in Settings. Idempotent: re-running overwrites in place.
pub fn install(
    fs: &mut dyn FileWriter,
    registry: &mut dyn RegistryWriter,
    exe_path: &Path,
    home: &Path,
) -> Result<Vec<PathBuf>, String> {
    let mut written = Vec::new();
    for browser in Browser::ALL {
        let path = manifest_file_path(home, browser);
        if let Some(parent) = path.parent() {
            fs.create_dir_all(parent)?;
        }
        fs.write(&path, &manifest_json(browser, exe_path))?;
        written.push(path.clone());
        #[cfg(target_os = "windows")]
        registry.set_default_value(&registry_subkey(browser), &path.display().to_string())?;
        #[cfg(not(target_os = "windows"))]
        let _ = &registry; // no registry on this platform; the file alone registers the host
    }
    Ok(written)
}

/// Removes exactly what `install` would have created for this `home` --
/// nothing else. Never errors on an already-absent file/key so repeated or
/// partial disconnects are safe.
pub fn uninstall(
    fs: &mut dyn FileWriter,
    registry: &mut dyn RegistryWriter,
    home: &Path,
) -> Result<(), String> {
    for browser in Browser::ALL {
        let path = manifest_file_path(home, browser);
        fs.remove(&path)?;
        #[cfg(target_os = "windows")]
        registry.delete_key(&registry_subkey(browser))?;
        #[cfg(not(target_os = "windows"))]
        let _ = &registry;
    }
    Ok(())
}

/// Whether every host manifest `install` would create is currently present
/// (used by Settings to show "connected" vs "not connected"). This checks
/// files only (not the Windows registry, which the real registration keeps
/// in lockstep) so it works identically for tests against a fake filesystem.
#[allow(dead_code)] // simple boolean form of `installed_browsers`; kept for tests/API symmetry
pub fn is_installed(fs: &dyn FileWriter, home: &Path) -> bool {
    Browser::ALL
        .iter()
        .all(|&browser| fs.exists(&manifest_file_path(home, browser)))
}

/// Which specific browsers currently have a host manifest, for a friendlier
/// Settings display ("registered: Chrome, Edge, Firefox") than a single
/// yes/no.
pub fn installed_browsers(fs: &dyn FileWriter, home: &Path) -> Vec<Browser> {
    Browser::ALL
        .into_iter()
        .filter(|&browser| fs.exists(&manifest_file_path(home, browser)))
        .collect()
}

/// Real (non-fake) implementations. `RealFileWriter` is plain `std::fs` and
/// works on every platform; `RealRegistryWriter` only does anything on
/// Windows (elsewhere `install`/`uninstall` never call it, per the
/// `cfg(not(target_os = "windows"))` branches above, but every platform
/// needs *some* type satisfying `RegistryWriter` to call them with).
pub mod real {
    use super::{FileWriter, RegistryWriter};
    use std::path::Path;

    pub struct RealFileWriter;
    impl FileWriter for RealFileWriter {
        fn create_dir_all(&mut self, path: &Path) -> Result<(), String> {
            std::fs::create_dir_all(path).map_err(|e| e.to_string())
        }
        fn write(&mut self, path: &Path, contents: &[u8]) -> Result<(), String> {
            std::fs::write(path, contents).map_err(|e| e.to_string())
        }
        fn remove(&mut self, path: &Path) -> Result<(), String> {
            match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        }
        fn exists(&self, path: &Path) -> bool {
            path.exists()
        }
    }

    #[cfg(not(target_os = "windows"))]
    pub struct RealRegistryWriter;
    #[cfg(not(target_os = "windows"))]
    impl RegistryWriter for RealRegistryWriter {
        fn set_default_value(&mut self, _subkey: &str, _manifest_path: &str) -> Result<(), String> {
            Ok(()) // never called: install()'s registry branch is windows-only
        }
        fn delete_key(&mut self, _subkey: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[cfg(target_os = "windows")]
    use windows::core::PCWSTR;
    #[cfg(target_os = "windows")]
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    #[cfg(target_os = "windows")]
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    #[cfg(target_os = "windows")]
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    #[cfg(target_os = "windows")]
    pub struct RealRegistryWriter;
    #[cfg(target_os = "windows")]
    impl RegistryWriter for RealRegistryWriter {
        fn set_default_value(&mut self, subkey: &str, manifest_path: &str) -> Result<(), String> {
            let subkey_w = wide(subkey);
            let mut key = HKEY::default();
            let status = unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    PCWSTR(subkey_w.as_ptr()),
                    None,
                    None,
                    REG_OPTION_NON_VOLATILE,
                    KEY_WRITE,
                    None,
                    &mut key,
                    None,
                )
            };
            if status != ERROR_SUCCESS {
                return Err(format!("RegCreateKeyExW failed: {status:?}"));
            }
            let value = wide(manifest_path);
            let bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(value.as_ptr() as *const u8, value.len() * 2)
            };
            let set = unsafe { RegSetValueExW(key, PCWSTR::null(), None, REG_SZ, Some(bytes)) };
            unsafe {
                let _ = RegCloseKey(key);
            }
            if set != ERROR_SUCCESS {
                return Err(format!("RegSetValueExW failed: {set:?}"));
            }
            Ok(())
        }
        fn delete_key(&mut self, subkey: &str) -> Result<(), String> {
            let subkey_w = wide(subkey);
            let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(subkey_w.as_ptr())) };
            if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND {
                return Err(format!("RegDeleteTreeW failed: {status:?}"));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
#[path = "browser_registration_tests.rs"]
mod tests;
