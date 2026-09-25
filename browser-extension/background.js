// Background service worker (Chrome/Edge) / event page (Firefox). Relays
// meeting-signal messages from content scripts to the desktop app's native
// messaging host. Never opens a network socket; native messaging is stdio
// between the browser and the app's own executable, managed entirely by the
// browser per the host manifest registered by "Connect browser extension" in
// the app's Settings. See docs/browser-extension.md.

import { opaqueTabId } from "./tab-id.js";

const NATIVE_HOST_NAME = "com.local.meetingrecorder.browser_bridge";
const MAX_MESSAGES_PER_MINUTE = 30; // generous headroom over one heartbeat/15s/tab

let port = null;
let sentThisWindow = 0;
let windowStart = Date.now();

function connect() {
  if (port) return port;
  try {
    port = chrome.runtime.connectNative(NATIVE_HOST_NAME);
    port.onDisconnect.addListener(() => {
      port = null; // reconnect lazily on the next message
    });
  } catch {
    port = null;
  }
  return port;
}

function withinRateLimit() {
  const now = Date.now();
  if (now - windowStart > 60_000) {
    windowStart = now;
    sentThisWindow = 0;
  }
  if (sentThisWindow >= MAX_MESSAGES_PER_MINUTE) return false;
  sentThisWindow += 1;
  return true;
}

async function relay(message, tabId) {
  if (!withinRateLimit()) return;
  const p = connect();
  if (!p) return;
  const opaque = await opaqueTabId(tabId, chrome.storage.local, crypto.subtle);
  try {
    p.postMessage({
      platform: message.platform,
      state: message.state,
      title: message.title || null,
      tab_id: opaque,
    });
  } catch {
    port = null;
  }
}

chrome.runtime.onMessage.addListener((message, sender) => {
  if (message && message.kind === "meeting-signal" && sender.tab) {
    relay(message, sender.tab.id);
  }
});

// A tab closed outright (crash, explicit close, browser quit) skips the
// content script's beforeunload "ended" message entirely. We don't track
// per-tab platform here, so we can't send a well-formed "ended" for it; the
// app instead expires the in-call state itself once the heartbeat goes
// stale (see docs/browser-extension.md and src-tauri/src/browser_bridge.rs).
