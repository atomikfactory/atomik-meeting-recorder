import { invoke, preview } from "../api";
import { disabled, store } from "../store";
import type { HotkeyStatus } from "../types";
import { escapeAttr, escapeHtml, setTextIfChanged } from "../util/escape";

export const DEFAULT_HOTKEY = "CommandOrControl+Alt+R";

// A modifier-only key-down never finishes a combination on its own -- the
// user is still holding it down while reaching for the letter/number key.
const BARE_MODIFIER_KEYS = new Set(["Control", "Alt", "Shift", "Meta", "OS", "AltGraph"]);

/** Windows-friendly display form of an accelerator this app stores/sends to
 * the backend verbatim (e.g. "CommandOrControl+Alt+R" -> "Ctrl+Alt+R"). Purely
 * cosmetic; validation and registration always use the raw stored string. */
export function displayAccelerator(raw: string): string {
  if (!raw) return "None set";
  return raw
    .split("+")
    .map((part) => {
      const p = part.trim();
      if (/^commandorcontrol$/i.test(p)) return "Ctrl";
      if (/^command$/i.test(p)) return "Cmd";
      if (/^control$/i.test(p)) return "Ctrl";
      if (/^super$/i.test(p)) return "Win";
      return p;
    })
    .join("+");
}

function keyToken(event: KeyboardEvent): string | null {
  if (BARE_MODIFIER_KEYS.has(event.key)) return null;
  if (event.key === " ") return "Space";
  if (event.key.length === 1) return event.key.toUpperCase();
  return event.key;
}

function stopListening() {
  store.hotkeyListening = false;
  document.removeEventListener("keydown", onCaptureKeydown, true);
}

function onCaptureKeydown(event: KeyboardEvent) {
  if (!store.hotkeyListening) return;
  event.preventDefault();
  event.stopPropagation();
  if (event.key === "Escape") {
    store.hotkeyDraft = store.settings.hotkey ?? DEFAULT_HOTKEY;
    store.hotkeyCaptureError = null;
    stopListening();
    updateHotkeyUI();
    return;
  }
  if (event.key === "Backspace") {
    store.hotkeyDraft = "";
    store.hotkeyCaptureError = null;
    stopListening();
    updateHotkeyUI();
    return;
  }
  const modifiers: string[] = [];
  if (event.ctrlKey) modifiers.push("Control");
  if (event.altKey) modifiers.push("Alt");
  if (event.shiftKey) modifiers.push("Shift");
  if (event.metaKey) modifiers.push("Super");
  const key = keyToken(event);
  if (!key) return; // just a modifier held down so far -- keep listening
  if (modifiers.length === 0) {
    store.hotkeyCaptureError = "Include at least one modifier key (Ctrl, Alt, Shift, or Win).";
    updateHotkeyUI();
    return; // keep listening; this key-down alone cannot be the shortcut
  }
  store.hotkeyDraft = [...modifiers, key].join("+");
  store.hotkeyCaptureError = null;
  stopListening();
  updateHotkeyUI();
}

export function hotkeyStatusText(): string {
  const state = store.hotkeyState;
  if (!state.enabled) return "Global shortcut is off. Recording still works from the window or tray.";
  if (state.error) return state.error;
  if (state.registered) return `Registered: ${displayAccelerator(state.hotkey)}. Works even while the window is hidden.`;
  return "Checking registration…";
}

export function updateHotkeyUI() {
  const button = document.querySelector<HTMLButtonElement>("#hotkey-capture");
  if (button) {
    button.textContent = store.hotkeyListening ? "Press keys… (Esc to cancel)" : displayAccelerator(store.hotkeyDraft);
    button.disabled = preview || !store.backendReady || store.hotkeyListening;
    button.setAttribute("aria-pressed", String(store.hotkeyListening));
  }
  const hidden = document.querySelector<HTMLInputElement>("input[name='hotkey']");
  if (hidden && hidden.value !== store.hotkeyDraft) hidden.value = store.hotkeyDraft;
  setTextIfChanged(document.querySelector("#hotkey-capture-error"), store.hotkeyCaptureError ?? "");
  setTextIfChanged(document.querySelector("#hotkey-status"), hotkeyStatusText());
}

export async function refreshHotkeyState() {
  if (preview || !store.backendReady || document.hidden || store.hotkeyPoll) return;
  store.hotkeyPoll = true;
  try {
    store.hotkeyState = await invoke<HotkeyStatus>("get_hotkey_status");
  } catch {
    /* best-effort only, like the browser-extension status poll */
  } finally {
    store.hotkeyPoll = false;
    updateHotkeyUI();
  }
}

export function hotkeySettingsSectionHtml(): string {
  const initial = store.hotkeyDraft || store.settings.hotkey || DEFAULT_HOTKEY;
  return `<section class="model-settings" aria-labelledby="hotkey-heading">
    <label class="toggle"
      ><input type="checkbox" name="hotkey_enabled" ${store.settings.hotkey_enabled !== false ? "checked" : ""} ${disabled()}/><span
        ><b id="hotkey-heading">Global recording shortcut</b><small
          >Toggles recording from anywhere on this computer, even while the window is hidden in the tray. If a detected meeting is waiting for your approval, pressing it approves that recording instead of starting a new one.</small
        ></span
      ></label
    ><input type="hidden" name="hotkey" value="${escapeAttr(initial)}" />
    <div class="hotkey-capture-row">
      <button type="button" class="secondary" id="hotkey-capture" aria-pressed="false" ${disabled()}>${escapeHtml(displayAccelerator(initial))}</button>
      <small>Click the button, then press your shortcut. Escape cancels; Backspace clears it.</small>
    </div>
    <p id="hotkey-capture-error" role="alert">${escapeHtml(store.hotkeyCaptureError ?? "")}</p>
    <p id="hotkey-status" role="status">${escapeHtml(hotkeyStatusText())}</p>
    <small>Reserved combinations such as Ctrl+Alt+Delete, Alt+F4, and Win+L are rejected. On Linux under Wayland, global shortcuts may not register at all -- registration failures are reported above, not silently ignored.</small>
  </section>`;
}

export function wireHotkeySection() {
  document.querySelector<HTMLButtonElement>("#hotkey-capture")?.addEventListener("click", () => {
    if (store.hotkeyListening || preview || !store.backendReady) return;
    store.hotkeyListening = true;
    store.hotkeyCaptureError = null;
    updateHotkeyUI();
    document.addEventListener("keydown", onCaptureKeydown, true);
  });
}

/** Keeps the capture field's draft in sync with whatever was actually
 * persisted, called after both a successful and a failed save so a stale
 * draft never lingers -- see settings/index.ts's submit handler. */
export function syncHotkeyDraftFromSettings() {
  store.hotkeyDraft = store.settings.hotkey ?? DEFAULT_HOTKEY;
  store.hotkeyCaptureError = null;
  updateHotkeyUI();
}
