/// Best-effort, deliberately narrow window-title hint. It never reads browser
/// profiles or media, and absence/ambiguity is represented as `None`.
pub(crate) fn best_effort_title() -> Option<String> {
    platform_title()
}

#[derive(Clone)]
struct Candidate {
    title: String,
    foreground: bool,
}
fn choose(candidates: Vec<Candidate>) -> Option<String> {
    let foreground: Vec<_> = candidates.iter().filter(|c| c.foreground).collect();
    let selected = if foreground.len() == 1 {
        foreground[0]
    } else if candidates.len() == 1 {
        &candidates[0]
    } else {
        return None;
    };
    let title = selected.title.trim();
    (!title.is_empty()).then(|| title.chars().take(160).collect())
}
fn recognized(executable: &str, title: &str) -> bool {
    let title = title.to_ascii_lowercase();
    (matches!(executable, "chrome.exe" | "firefox.exe")
        && (title.contains("google meet") || title.starts_with("meet - ")))
        || (executable == "slack.exe"
            && (title.contains("huddle")
                || title.starts_with("call with ")
                || title.contains(" | call")))
}

#[cfg(not(target_os = "windows"))]
fn platform_title() -> Option<String> {
    None
}

#[cfg(target_os = "windows")]
fn platform_title() -> Option<String> {
    use windows::{
        core::{BOOL, PWSTR},
        Win32::{
            Foundation::{CloseHandle, HWND, LPARAM},
            System::Threading::{
                OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
                PROCESS_QUERY_LIMITED_INFORMATION,
            },
            UI::WindowsAndMessaging::{
                EnumWindows, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
                IsWindowVisible,
            },
        },
    };
    struct Scan {
        foreground: HWND,
        candidates: Vec<Candidate>,
    }
    unsafe extern "system" fn visit(hwnd: HWND, value: LPARAM) -> BOOL {
        let scan = unsafe { &mut *(value.0 as *mut Scan) };
        if unsafe { IsWindowVisible(hwnd) } == false {
            return true.into();
        }
        let mut buffer = [0u16; 512];
        let count = unsafe { GetWindowTextW(hwnd, &mut buffer) };
        if count <= 0 {
            return true.into();
        }
        let title = String::from_utf16_lossy(&buffer[..count as usize]);
        let lower = title.to_ascii_lowercase();
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        let process = match executable(pid) {
            Some(process) => process,
            None => return true.into(),
        };
        if recognized(&process, &lower) {
            scan.candidates.push(Candidate {
                title,
                foreground: hwnd == scan.foreground,
            });
        }
        true.into()
    }
    fn executable(pid: u32) -> Option<String> {
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
        let mut path = [0u16; 32_768];
        let mut len = path.len() as u32;
        let ok = unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(path.as_mut_ptr()),
                &mut len,
            )
        }
        .is_ok();
        let _ = unsafe { CloseHandle(process) };
        if !ok {
            return None;
        }
        String::from_utf16_lossy(&path[..len as usize])
            .rsplit(['\\', '/'])
            .next()
            .map(|x| x.to_ascii_lowercase())
    }
    let mut scan = Scan {
        foreground: unsafe { GetForegroundWindow() },
        candidates: vec![],
    };
    let _ = unsafe { EnumWindows(Some(visit), LPARAM((&mut scan as *mut Scan) as isize)) };
    choose(scan.candidates)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ambiguous_titles_are_not_stored() {
        assert_eq!(
            choose(vec![
                Candidate {
                    title: "A".into(),
                    foreground: false
                },
                Candidate {
                    title: "B".into(),
                    foreground: false
                }
            ]),
            None
        );
        assert_eq!(
            choose(vec![
                Candidate {
                    title: "A".into(),
                    foreground: true
                },
                Candidate {
                    title: "B".into(),
                    foreground: false
                }
            ]),
            Some("A".into())
        );
    }
}
