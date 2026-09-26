import { listen } from "@tauri-apps/api/event";
import "./style.css";
import "./overrides.css";

import { invoke, preview } from "./api";
import { notice } from "./ui/notice";
import { icon, store } from "./store";
import type { AppState, Settings } from "./types";
import { refreshDeviceSection } from "./settings/audio";
import { refreshTranscriptionState, updateTranscriptionUI } from "./settings/transcription";
import { refreshDiarizationState } from "./settings/speakers";
import { refreshSummaryState, refreshSummaryModels, updateSummarySettingsUI } from "./settings/summary";
import { refreshCompressionState, refreshAudioStorage } from "./settings/compression";
import { refreshBrowserExtensionState } from "./settings/extension";
import { refreshAppFlowyState, refreshAppFlowyWorkspaces } from "./settings/appflowy";
import { refreshHotkeyState, updateHotkeyUI } from "./settings/hotkey";
import { settingsSectionHtml, wireSettingsSection } from "./settings/index";
import { recentRecordings, refreshRecordingState, recordingLabel } from "./views/library";
import { dashboardSectionHtml, refreshDetectionState, updateDetectionUI, wireDashboardSection } from "./views/dashboard";
import { wireTranscriptSearch } from "./views/search";
import { deviceRows, refreshDevices } from "./settings/audio";

async function loadDesktop() {
  try {
    const [saved, app] = await Promise.all([
      invoke<Settings>("get_settings"),
      invoke<AppState>("get_app_state"),
    ]);
    store.settings = { ...saved, audio_sources: saved.audio_sources ?? [] };
    store.selectedSourceIds = new Set(store.settings.audio_sources.map((source) => source.id));
    store.draftSources = new Map(store.settings.audio_sources.map((source) => [source.id, source]));
    store.hotkeyDraft = store.settings.hotkey ?? store.hotkeyDraft;
    store.appState = app;
    store.backendReady = true;
    render();
    refreshDevices();
    refreshRecordingState();
    refreshDetectionState();
    refreshTranscriptionState();
    refreshDiarizationState();
    refreshSummaryState();
    refreshCompressionState();
    refreshAudioStorage();
    refreshBrowserExtensionState();
    refreshAppFlowyState().then(refreshAppFlowyWorkspaces);
    refreshSummaryModels(store.settings.ollama_url ?? "http://127.0.0.1:11434");
    refreshHotkeyState();
  } catch (error) {
    store.backendReady = false;
    render();
    notice(`Desktop connection failed: ${String(error)}`, "error");
  }
}

function render() {
  document.querySelector<HTMLDivElement>("#app")!.innerHTML =
    /* HTML */ `<aside>
        <div class="brand">
          ${icon}<span>meeting<br /><b>recorder</b></span>
        </div>
        <nav>
          <button class="active" data-page="dashboard">
            <span>Dashboard</span></button
          ><button data-page="settings"><span>Settings</span></button>
        </nav>
        <div class="side-note">
          Local-first by design<br /><small>No account required</small>
        </div>
      </aside>
      <main>
        ${preview ? `<div class="preview">Browser preview only. Desktop actions and saving are disabled.</div>` : ""}${!preview && !store.backendReady ? `<button id="retry" class="preview">Retry desktop connection</button>` : ""}
        <div class="topbar">
          <span>YOUR WORKSPACE</span
          ><span id="top-recording-status" class="status-dot">${recordingLabel(store.recordingState.status)}</span>
        </div>
        ${dashboardSectionHtml()}
        ${settingsSectionHtml()}
        <dialog id="management-dialog" aria-labelledby="management-title"><form method="dialog"><h2 id="management-title">Manage meeting</h2><label id="management-name-label">Meeting title<input id="management-name" /></label><p id="management-detail"></p><p id="management-error" role="alert"></p><menu><button id="management-cancel" value="cancel">Cancel</button><button id="management-confirm" value="default">Confirm</button></menu></form></dialog><dialog id="bulk-dialog" aria-labelledby="bulk-dialog-title"><form method="dialog"><h2 id="bulk-dialog-title"></h2><p id="bulk-dialog-detail"></p><div id="bulk-delete-options" hidden><label><input type="radio" name="bulk-mode" value="recycle_bin" checked/> Move to Recycle Bin (recoverable)</label><label><input type="radio" name="bulk-mode" value="permanent"/> Delete permanently (cannot be undone)</label><label id="bulk-confirm-label" hidden>Type <b id="bulk-confirm-target">DELETE</b> (or the number of meetings) to confirm<input id="bulk-confirm-input" autocomplete="off"/></label></div><p id="bulk-dialog-error" role="alert"></p><menu><button id="bulk-dialog-cancel" value="cancel">Cancel</button><button id="bulk-dialog-confirm" value="default">Confirm</button></menu></form></dialog><p id="toast" role="alert" aria-live="polite"></p>
      </main>`;
  document.querySelectorAll<HTMLButtonElement>("[data-page]").forEach(
    (button) =>
      (button.onclick = () => {
        document
          .querySelectorAll(".page")
          .forEach((page) => page.classList.add("hidden"));
        document
          .getElementById(button.dataset.page!)!
          .classList.remove("hidden");
        document
          .querySelectorAll("nav button")
          .forEach((item) => item.classList.remove("active"));
        button.classList.add("active");
      }),
  );
  document.querySelectorAll<HTMLInputElement>("input[type=range]").forEach(
    (input) =>
      (input.oninput = () => {
        const label = document.querySelector<HTMLElement>(
          `[data-for='${input.name}']`,
        );
        if (label) label.textContent = `${input.value} seconds`;
      }),
  );
  wireDashboardSection();
  document.querySelector("#retry")?.addEventListener("click", loadDesktop);
  document.querySelector("#refresh-devices")?.addEventListener("click", refreshDevices);
  store.renderedDeviceRows = deviceRows();
  store.renderedRecentRecordings = recentRecordings();
  refreshDeviceSection();
  updateDetectionUI();
  updateTranscriptionUI();
  updateSummarySettingsUI();
  updateHotkeyUI();
  wireSettingsSection();
  wireTranscriptSearch();
}

if (!preview) {
  listen("detection-state", () => {
    refreshDetectionState();
    // Navigate without rendering again, preserving any unsaved settings draft.
    document.querySelector<HTMLButtonElement>("[data-page='dashboard']")?.click();
  });
  listen("recording-unavailable", () => notice("Recording could not start. No audio was captured.", "error"));
  listen<string>("recording-failed", (event) => {
    notice(`Recording error: ${event.payload}`, "error");
    // A close request can have hidden the app before background finalization
    // discovers an error. Bring the error into the user's normal polling UI.
    refreshRecordingState();
  });
  listen("navigate-settings", () =>
    document
      .querySelector<HTMLButtonElement>("[data-page='settings']")
      ?.click(),
  );
  loadDesktop();
  window.setInterval(refreshDevices, 5000);
  window.setInterval(() => refreshRecordingState(), 100);
  window.setInterval(refreshTranscriptionState, 1000);
  window.setInterval(refreshDiarizationState, 1000);
  window.setInterval(refreshSummaryState, 1000);
  window.setInterval(refreshCompressionState, 1000);
  window.setInterval(refreshAudioStorage, 5000);
  window.setInterval(refreshDetectionState, 1000);
  window.setInterval(refreshBrowserExtensionState, 5000);
  window.setInterval(refreshAppFlowyState, 2000);
  window.setInterval(refreshHotkeyState, 1000);
  window.addEventListener("focus", refreshDevices);
  window.addEventListener("focus", () => { refreshRecordingState(); refreshTranscriptionState(); refreshDiarizationState(); refreshSummaryState(); refreshCompressionState(); refreshAudioStorage(); refreshBrowserExtensionState(); refreshAppFlowyState(); refreshHotkeyState(); });
  window.addEventListener("focus", refreshDetectionState);
  document.addEventListener("visibilitychange", () => { if (!document.hidden) { refreshDevices(); refreshRecordingState(); refreshDetectionState(); refreshTranscriptionState(); refreshSummaryState(); refreshCompressionState(); refreshAudioStorage(); refreshBrowserExtensionState(); refreshAppFlowyState(); refreshHotkeyState(); } });
} else render();
