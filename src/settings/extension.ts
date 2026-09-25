import { invoke, preview } from "../api";
import { notice } from "../ui/notice";
import { store } from "../store";
import type { BrowserExtensionStatus } from "../types";
import { setTextIfChanged } from "../util/escape";

export async function refreshBrowserExtensionState() {
  if (preview || !store.backendReady || document.hidden || store.browserExtensionPoll) return;
  store.browserExtensionPoll = true;
  try {
    store.browserExtensionState = await invoke<BrowserExtensionStatus>("get_browser_extension_status");
  } catch {
    /* best-effort only */
  } finally {
    store.browserExtensionPoll = false;
    updateBrowserExtensionUI();
  }
}

export function browserExtensionStatusText(): string {
  if (!store.browserExtensionState.registered) return "Not connected. Click Connect, then load the extension in your browser.";
  if (!store.browserExtensionState.connected) return `Registered for ${store.browserExtensionState.browsers.join(", ")}. Waiting for the browser extension to report in.`;
  const ago = store.browserExtensionState.last_message_seconds_ago;
  const freshness = ago == null ? "" : ago < 60 ? ` (last message ${ago}s ago)` : ` (last message ${Math.round(ago / 60)}m ago -- may be idle or disconnected)`;
  return `Connected via ${store.browserExtensionState.browsers.join(", ")}${freshness}.`;
}

export function updateBrowserExtensionUI() {
  setTextIfChanged(document.querySelector("#browser-extension-status"), browserExtensionStatusText());
  const connectBtn = document.querySelector<HTMLButtonElement>("#connect-browser-extension");
  const disconnectBtn = document.querySelector<HTMLButtonElement>("#disconnect-browser-extension");
  if (connectBtn) connectBtn.disabled = preview || !store.backendReady || store.browserExtensionActionInFlight;
  if (disconnectBtn) disconnectBtn.disabled = preview || !store.backendReady || store.browserExtensionActionInFlight || !store.browserExtensionState.registered;
}

export function wireExtensionSection() {
  document.querySelector("#connect-browser-extension")?.addEventListener("click", async () => {
    if (store.browserExtensionActionInFlight) return;
    store.browserExtensionActionInFlight = true; updateBrowserExtensionUI();
    try {
      store.browserExtensionState = await invoke<BrowserExtensionStatus>("connect_browser_extension");
      notice("Browser extension host registered. Load the extension in your browser to finish connecting.", "status");
    } catch (error) {
      notice(String(error), "error");
    } finally {
      store.browserExtensionActionInFlight = false; updateBrowserExtensionUI();
    }
  });
  document.querySelector("#disconnect-browser-extension")?.addEventListener("click", async () => {
    if (store.browserExtensionActionInFlight) return;
    store.browserExtensionActionInFlight = true; updateBrowserExtensionUI();
    try {
      store.browserExtensionState = await invoke<BrowserExtensionStatus>("disconnect_browser_extension");
      notice("Browser extension host unregistered.", "status");
    } catch (error) {
      notice(String(error), "error");
    } finally {
      store.browserExtensionActionInFlight = false; updateBrowserExtensionUI();
    }
  });
  document.querySelector("#open-browser-extension-folder")?.addEventListener("click", () =>
    invoke("open_browser_extension_folder").catch((error) => notice(String(error), "error")),
  );
}
