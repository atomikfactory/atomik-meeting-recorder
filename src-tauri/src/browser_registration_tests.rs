use super::*;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[derive(Default)]
struct FakeFs {
    files: HashMap<PathBuf, Vec<u8>>,
    dirs: HashSet<PathBuf>,
}
impl FileWriter for FakeFs {
    fn create_dir_all(&mut self, path: &Path) -> Result<(), String> {
        self.dirs.insert(path.to_path_buf());
        Ok(())
    }
    fn write(&mut self, path: &Path, contents: &[u8]) -> Result<(), String> {
        self.files.insert(path.to_path_buf(), contents.to_vec());
        Ok(())
    }
    fn remove(&mut self, path: &Path) -> Result<(), String> {
        self.files.remove(path);
        Ok(()) // idempotent, matches RealFileWriter's NotFound-is-ok behavior
    }
    fn exists(&self, path: &Path) -> bool {
        self.files.contains_key(path)
    }
}

#[derive(Default)]
struct FakeRegistry {
    values: HashMap<String, String>,
}
impl RegistryWriter for FakeRegistry {
    fn set_default_value(&mut self, subkey: &str, manifest_path: &str) -> Result<(), String> {
        self.values.insert(subkey.to_string(), manifest_path.to_string());
        Ok(())
    }
    fn delete_key(&mut self, subkey: &str) -> Result<(), String> {
        self.values.remove(subkey);
        Ok(()) // idempotent
    }
}

fn home() -> PathBuf {
    PathBuf::from("/home/test-user")
}

#[test]
fn install_writes_a_manifest_for_every_browser() {
    let mut fs = FakeFs::default();
    let mut registry = FakeRegistry::default();
    let written = install(&mut fs, &mut registry, Path::new("/opt/meeting-recorder"), &home()).unwrap();
    assert_eq!(written.len(), Browser::ALL.len());
    for path in &written {
        assert!(fs.exists(path));
    }
}

#[test]
fn install_is_idempotent() {
    let mut fs = FakeFs::default();
    let mut registry = FakeRegistry::default();
    let exe = Path::new("/opt/meeting-recorder");
    let first = install(&mut fs, &mut registry, exe, &home()).unwrap();
    let second = install(&mut fs, &mut registry, exe, &home()).unwrap();
    assert_eq!(first, second);
    assert_eq!(fs.files.len(), Browser::ALL.len());
}

#[test]
fn chrome_and_edge_manifests_pin_the_chromium_extension_id_via_allowed_origins() {
    let mut fs = FakeFs::default();
    let mut registry = FakeRegistry::default();
    // install() writes in Browser::ALL order: Chrome, Edge, Firefox.
    let written = install(&mut fs, &mut registry, Path::new("/opt/app"), &home()).unwrap();
    for manifest in &written[0..2] {
        let body = String::from_utf8(fs.files.get(manifest).unwrap().clone()).unwrap();
        assert!(body.contains(CHROMIUM_EXTENSION_ID));
        assert!(body.contains("allowed_origins"));
        assert!(!body.contains("allowed_extensions"));
    }
}

#[test]
fn firefox_manifest_uses_allowed_extensions_with_the_gecko_id() {
    let mut fs = FakeFs::default();
    let mut registry = FakeRegistry::default();
    let written = install(&mut fs, &mut registry, Path::new("/opt/app"), &home()).unwrap();
    let firefox_manifest = &written[2]; // Browser::ALL's third entry
    let body = String::from_utf8(fs.files.get(firefox_manifest).unwrap().clone()).unwrap();
    assert!(body.contains(FIREFOX_EXTENSION_ID));
    assert!(body.contains("allowed_extensions"));
    assert!(!body.contains("allowed_origins"));
}

#[test]
fn every_manifest_points_at_the_given_executable_path() {
    let mut fs = FakeFs::default();
    let mut registry = FakeRegistry::default();
    let exe = Path::new("/opt/custom/meeting-recorder");
    let written = install(&mut fs, &mut registry, exe, &home()).unwrap();
    for path in &written {
        let body = String::from_utf8(fs.files.get(path).unwrap().clone()).unwrap();
        assert!(body.contains("meeting-recorder"), "manifest missing exe path: {body}");
    }
}

#[test]
fn is_installed_is_false_until_install_runs_then_true() {
    let mut fs = FakeFs::default();
    let mut registry = FakeRegistry::default();
    assert!(!is_installed(&fs, &home()));
    install(&mut fs, &mut registry, Path::new("/opt/app"), &home()).unwrap();
    assert!(is_installed(&fs, &home()));
}

#[test]
fn uninstall_removes_exactly_what_install_created() {
    let mut fs = FakeFs::default();
    let mut registry = FakeRegistry::default();
    let exe = Path::new("/opt/app");
    let written = install(&mut fs, &mut registry, exe, &home()).unwrap();
    for path in &written {
        assert!(fs.exists(path));
    }
    uninstall(&mut fs, &mut registry, &home()).unwrap();
    for path in &written {
        assert!(!fs.exists(path));
    }
    assert!(!is_installed(&fs, &home()));
}

#[test]
fn uninstall_without_a_prior_install_does_not_error() {
    let mut fs = FakeFs::default();
    let mut registry = FakeRegistry::default();
    assert!(uninstall(&mut fs, &mut registry, &home()).is_ok());
}

#[test]
fn uninstall_does_not_touch_unrelated_files() {
    let mut fs = FakeFs::default();
    let mut registry = FakeRegistry::default();
    let unrelated = PathBuf::from("/home/test-user/.config/google-chrome/some-other-host.json");
    fs.write(&unrelated, b"keep me").unwrap();
    install(&mut fs, &mut registry, Path::new("/opt/app"), &home()).unwrap();
    uninstall(&mut fs, &mut registry, &home()).unwrap();
    assert!(fs.exists(&unrelated));
}
