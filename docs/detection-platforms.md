# Native detection platforms

Native meeting detection provides hints to the controller; it is not an
authoritative meeting-state source and it never starts recording by itself.
Detection requests are enabled by default, but the user's ask-first preference
requires explicit approval before each detected recording.

## Windows

The Windows provider runs its COM work on a dedicated worker thread. Each
bounded sample enumerates active WASAPI **capture** endpoints and their active
audio sessions, then reads only the owning process executable name. It does not
open an audio stream, activate a microphone or camera, inspect browser profiles,
or contact a network service. The returned snapshot contains only a stable
provider key and generic detail—never a process ID, executable path, or window
title.

Current conservative hints are:

| Hint | Required local observations |
| --- | --- |
| `slack` | An active capture session attributed to `slack.exe`. |
| `google_meet_chrome` | An active `chrome.exe` capture session and a visible Chrome-family window title indicating Google Meet. |
| `google_meet_firefox` | An active `firefox.exe` capture session and a visible Firefox-family window title indicating Google Meet. |

Executable attribution is heuristic. Windows documents that an audio session
can span processes, in which case `GetProcessId` returns the initial owner;
browser capture may also be hosted by a utility process while a top-level window
is owned by another browser process. The provider therefore correlates only the
browser executable family, not a retained PID. A browser microphone session on
its own is never a Google Meet hint, and other browser media is not a meeting
hint.

The WASAPI session enumerator is a point-in-time list and Microsoft documents
that it can miss sessions newly reported through session notifications. This
provider creates a fresh manager/enumerator per sample and reports an `error`
(unknown, rather than absence) if an enumeration, attribution, process query, or
window scan cannot be completed. Future notification-based tracking could reduce
that race but would still remain a hint.

Muted calls, browser/UI title changes, background or hidden windows, device
changes, protected processes, and apps that use multiple processes can all cause
missed or ambiguous detection. A positive hint does not prove that a person is
currently speaking; it only indicates the stated local conditions.

Microsoft API references: [IAudioSessionEnumerator](https://learn.microsoft.com/en-us/windows/win32/api/audiopolicy/nn-audiopolicy-iaudiosessionenumerator), [IAudioSessionControl2::GetProcessId](https://learn.microsoft.com/en-us/windows/win32/api/audiopolicy/nf-audiopolicy-iaudiosessioncontrol2-getprocessid), [EnumWindows](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-enumwindows), and [GetWindowTextW](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getwindowtextw).

## macOS and Linux

The native provider explicitly returns `supported: false` on macOS and Linux.
No process, browser, microphone, or window inspection is performed there.
