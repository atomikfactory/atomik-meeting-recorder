import { invoke, preview } from "../api";
import { notice } from "../ui/notice";
import { disabled, store } from "../store";
import type { Language, Model, Settings } from "../types";
import { escapeAttr, escapeHtml } from "../util/escape";
import { deviceRows, wireAudioSourcesSection } from "./audio";
import { wireTranscriptionSection } from "./transcription";
import { wireSpeakersSection } from "./speakers";
import { wireSummarySection } from "./summary";
import { audioStorageSummaryText } from "./compression";
import { appflowySettingsSectionHtml, wireAppFlowySettingsSection } from "./appflowy";
import { browserExtensionStatusText, wireExtensionSection } from "./extension";
import { wireFolderSection, refreshAppState, refreshRecordingsFolderSection } from "./folder";
import { refreshDetectionState } from "../views/dashboard";
import { refreshTranscriptionState } from "./transcription";
import { refreshDiarizationState } from "./speakers";
import { refreshSummaryState } from "./summary";
import { refreshAppFlowyState } from "./appflowy";

export function settingsSectionHtml(): string {
  return `<section id="settings" class="page hidden">
          <p class="kicker">PREFERENCES</p>
          <h1>Settings</h1>
          <p class="lede">Saved locally on this computer.</p>
          <form id="settings-form">
            <label class="toggle"
              ><input
                type="checkbox"
                name="auto_record"
                ${store.settings.auto_record ? "checked" : ""}
                ${disabled()}
              /><span
                ><b>Detect meetings and ask to record</b
                ><small id="detection-settings-help"
                  >Asks before each recording. Manual recording is always available.</small
                ></span
              ></label
            ><label
              >Start debounce
              <span class="value" data-for="start_debounce_seconds"
                >${store.settings.start_debounce_seconds} seconds</span
              ><input
                name="start_debounce_seconds"
                type="range"
                min="0"
                max="60"
                value="${store.settings.start_debounce_seconds}"
                ${disabled()} /></label
            ><label
              >End grace period
              <span class="value" data-for="end_grace_seconds"
                >${store.settings.end_grace_seconds} seconds</span
              ><input
                name="end_grace_seconds"
                type="range"
                min="5"
                max="300"
                step="5"
                value="${store.settings.end_grace_seconds}"
                ${disabled()} /></label
            ><label
              >Whisper model<select name="whisper_model" ${disabled()}>
                ${(["tiny", "base", "small", "medium", "large", "turbo"] as Model[]).map((x) => `<option value="${x}" ${store.settings.whisper_model === x ? "selected" : ""}>${x}${x === "turbo" ? " (recommended)" : ""}${(store.transcriptionState.imported_models ?? []).includes(x) ? "" : " (not imported)"}</option>`).join("")}</select
              ><small
                >Used for local transcription after you choose an installed model.</small
              ><small id="model-select-warning" role="alert">${(store.transcriptionState.imported_models ?? []).includes(store.settings.whisper_model) ? "" : `${store.settings.whisper_model} is not imported yet; jobs will wait until you import it or pick an imported size.`}</small
              ><small
                >Recommended: <code>turbo</code> (large-v3-turbo), quantized — e.g. <code>ggml-large-v3-turbo-q5_0.bin</code> (~550 MB) from the whisper.cpp Hugging Face repo. It is far more accurate than <code>base</code> for a similar CPU cost.</small
              ></label
            ><label
              >Your name<input
                type="text"
                name="local_speaker_name"
                maxlength="64"
                value="${escapeAttr(store.settings.local_speaker_name ?? "You")}"
                ${disabled()} /><small
                >Labels your own microphone lines in transcripts and summaries. Remote participants are labeled "Speaker 1", "Speaker 2", … when speaker identification below has run, or "Remote" otherwise.</small
              ></label
            ><label
              >Transcription language<select name="transcription_language" ${disabled()}>
                ${([
                  ["auto", "Detect automatically"],
                  ["en", "English"],
                  ["fr", "French"],
                  ["es", "Spanish"],
                  ["de", "German"],
                  ["it", "Italian"],
                  ["pt", "Portuguese"],
                  ["nl", "Dutch"],
                ] as [Language, string][]).map(([value, label]) => `<option value="${value}" ${(store.settings.transcription_language ?? "auto") === value ? "selected" : ""}>${label}</option>`).join("")}</select
              ><small
                >"Detect automatically" identifies each audio source's language once, from its first speech, and keeps using it for the rest of the meeting.</small
              ></label
            ><label class="toggle"
              ><input type="checkbox" name="echo_cancellation" ${store.settings.echo_cancellation !== false ? "checked" : ""} ${disabled()}/><span
                ><b>Cancel speaker echo</b><small
                  >When a recording has both a microphone and a system-audio (loopback) source, removes remote participants' audio picked up a second time by the microphone before transcribing, so it does not appear twice. Has no effect on recordings with only one of those.</small
                ></span
              ></label
            ><label class="toggle"
              ><input type="checkbox" name="dedupe_echo_segments" ${store.settings.dedupe_echo_segments !== false ? "checked" : ""} ${disabled()}/><span
                ><b>Remove duplicate echoed lines</b><small
                  >After transcribing, drops a microphone line that closely matches an overlapping loopback line -- a text-level safety net that also helps when echo cancellation is off or does not fully catch it.</small
                ></span
              ></label
            >
            <section class="model-settings"><b>Local transcription model</b><p id="model-status">No local model selected</p><p id="backend-status">Backend: ${escapeHtml(store.transcriptionState.backend)}</p><button type="button" class="secondary" id="choose-model" ${disabled()}>Choose local Whisper model</button><small>Choose a GGML .bin file; model and audio stay local. No model downloads occur automatically.</small></section>
            <section class="model-settings"><label class="toggle"><input type="checkbox" name="diarization_enabled" ${store.settings.diarization_enabled !== false ? "checked" : ""} ${disabled()}/><span><b>Identify speakers locally</b><small>Runs on remote (loopback) audio only, labeling distinct voices "Speaker 1", "Speaker 2", … by first appearance, or "Remote" for uncertain turns. Numbers are not named identities and are not comparable across recordings.</small></span></label><p id="diarization-status">Choose both local models to identify speakers. Transcription remains available.</p><div><button type="button" class="secondary" data-diarization-kind="segmentation" ${disabled()}>Choose segmentation model</button> <button type="button" class="secondary" data-diarization-kind="embedding" ${disabled()}>Choose voice model</button></div></section>
            <section class="model-settings"><label class="toggle"><input type="checkbox" name="summary_enabled" ${store.settings.summary_enabled !== false ? "checked" : ""} ${disabled()}/><span><b>Summarize meetings locally with Ollama</b><small>Sends the finished transcript to a local Ollama server only (never the internet) for a title, summary, key points, decisions, action items, and open questions. Harmless when Ollama isn't running — summaries simply wait.</small></span></label>
              <label>Ollama URL<input type="text" name="ollama_url" value="${escapeAttr(store.settings.ollama_url ?? "http://127.0.0.1:11434")}" ${disabled()}/><small>Must be loopback (localhost/127.0.0.1/[::1]) so summaries can never leave this device.</small></label>
              <label>Summary model<select name="summary_model" ${disabled()}>${[...new Set([...store.summaryModels, store.settings.summary_model ?? "qwen3:8b"])].map((name) => `<option value="${escapeAttr(name)}" ${(store.settings.summary_model ?? "qwen3:8b") === name ? "selected" : ""}>${escapeHtml(name)}</option>`).join("")}</select></label>
              <p id="summary-models-status">${escapeHtml(store.summaryModelsError ?? "Refresh to list models installed in Ollama.")}</p>
              <div><button type="button" class="secondary" id="refresh-summary-models" ${disabled()}>Refresh models</button> <button type="button" class="secondary" id="test-ollama-connection" ${disabled()}>Test connection</button></div>
            </section>
            <section id="appflowy-settings-section" class="model-settings" aria-labelledby="appflowy-heading">${appflowySettingsSectionHtml()}</section>
            <section class="model-settings"><label class="toggle"><input type="checkbox" name="compress_audio" ${store.settings.compress_audio !== false ? "checked" : ""} ${disabled()}/><span><b>Compress finished recordings to FLAC</b><small>Once transcription (and speaker identification, if enabled) has finished with a recording's audio, losslessly compresses each WAV track to FLAC and verifies it decodes back to the exact original samples before removing the WAV. Never runs on an active recording.</small></span></label>
              <p id="audio-storage-total">${escapeHtml(audioStorageSummaryText())}</p>
            </section>
            <section class="model-settings" aria-labelledby="recordings-folder-heading">
              <b id="recordings-folder-heading">Recordings folder</b>
              <small>Where meeting audio, transcripts, and summaries are stored. Databases and imported models always stay in the app's own data folder.</small>
              <p id="recordings-folder-path">${escapeHtml(store.appState.recordings_folder)}</p>
              <div>
                <button type="button" class="secondary" id="choose-recordings-folder" ${disabled()}>Change…</button>
                <button type="button" class="secondary" id="reset-recordings-folder" ${preview || !store.backendReady || !store.settings.recordings_folder ? 'disabled aria-disabled="true"' : ""}>Reset to default</button>
                <button type="button" class="secondary" id="open-recordings-folder-btn">Open folder</button>
              </div>
              <div>
                <button type="button" class="secondary" id="move-recordings" ${disabled()}>Move existing recordings here</button>
                <small>Not automatic: copies each recording, verifies it byte-for-byte, then removes it from its old folder. Safe to retry if interrupted.</small>
              </div>
              <p id="recordings-folder-status" role="status"></p>
            </section>
            <section class="audio-settings" aria-labelledby="audio-sources-heading">
              <div class="audio-head"><div><b id="audio-sources-heading">Audio sources</b><small>Select one or more inputs for local capture.</small></div><button type="button" id="refresh-devices" class="secondary" ${disabled()}>Refresh</button></div>
              <p id="capture-next" class="capture-next">Select one or more sources for your next local recording.</p>
              <div id="device-list">${deviceRows()}</div><p id="inventory-backend" class="inventory-backend">${store.inventory?.backend ? `Backend: ${escapeHtml(store.inventory.backend)}` : "Backend unavailable"}</p><p id="device-errors" class="device-errors" role="alert" aria-live="assertive"></p><button class="primary" type="button" id="save-audio-sources" ${disabled()}>Save audio sources</button>
            </section>
            <section class="model-settings" aria-labelledby="browser-extension-heading">
              <b id="browser-extension-heading">Browser extension</b>
              <small>A local-only Chrome/Edge/Firefox extension detects Google Meet (and Teams/Zoom) calls, including background tabs, and sends only the platform, call state, a title, and an opaque tab id to this app over native messaging -- never URLs, participant names, or page content. Recording still always asks for your approval first.</small>
              <p id="browser-extension-status" role="status">${escapeHtml(browserExtensionStatusText())}</p>
              <div>
                <button type="button" class="secondary" id="connect-browser-extension" ${preview || !store.backendReady ? "disabled" : ""}>Connect browser extension</button>
                <button type="button" class="secondary" id="disconnect-browser-extension" ${preview || !store.backendReady || !store.browserExtensionState.registered ? "disabled" : ""}>Disconnect</button>
                <button type="button" class="secondary" id="open-browser-extension-folder">Open extension folder</button>
              </div>
              <small>After connecting: in Chrome/Edge, open chrome://extensions (or edge://extensions), enable Developer mode, and "Load unpacked" the opened folder. In Firefox, open about:debugging#/runtime/this-firefox and "Load Temporary Add-on…", selecting manifest.firefox.json's folder. See docs/browser-extension.md.</small>
            </section>
            <div class="integration">
              <b>AppFlowy</b><span>${store.appflowyState.enabled ? (store.appflowyState.connected ? "Connected" : "Signed out") : "Off"}</span
              ><small>${escapeHtml(store.appflowyState.enabled ? (store.appflowyState.connected ? `Exporting to ${store.appflowyState.email ?? "your workspace"}; see the AppFlowy export section below.` : "Enabled, but not signed in yet; see the AppFlowy export section below.") : "Off by default; enable it in the AppFlowy export section below.")}</small>
            </div>
            <button class="primary" type="submit" ${disabled()}>
              Save preferences
            </button>
            <p class="form-message" role="status"></p>
          </form>
        </section>`;
}

export function wireSettingsSection() {
  document.querySelector<HTMLFormElement>("#settings-form")!.onsubmit = async (event) => {
    event.preventDefault();
    const form = new FormData(event.currentTarget as HTMLFormElement);
    const next: Settings = {
      auto_record: form.get("auto_record") === "on",
      start_debounce_seconds: Number(form.get("start_debounce_seconds")),
      end_grace_seconds: Number(form.get("end_grace_seconds")),
      whisper_model: String(form.get("whisper_model")) as Model,
      diarization_enabled: form.get("diarization_enabled") === "on",
      transcription_language: String(form.get("transcription_language") ?? "auto") as Language,
      echo_cancellation: form.get("echo_cancellation") === "on",
      dedupe_echo_segments: form.get("dedupe_echo_segments") === "on",
      summary_enabled: form.get("summary_enabled") === "on",
      ollama_url: String(form.get("ollama_url") ?? "http://127.0.0.1:11434").trim(),
      summary_model: String(form.get("summary_model") ?? "qwen3:8b"),
      compress_audio: form.get("compress_audio") === "on",
      local_speaker_name: String(form.get("local_speaker_name") ?? "You").trim() || "You",
      appflowy_enabled: form.get("appflowy_enabled") === "on",
      appflowy_auto_export: form.get("appflowy_auto_export") === "on",
      appflowy_base_url: String(form.get("appflowy_base_url") ?? "https://beta.appflowy.cloud").trim(),
      appflowy_use_native_block: form.get("appflowy_use_native_block") === "on",
      appflowy_workspace_id: (form.get("appflowy_workspace_id") as string) || store.settings.appflowy_workspace_id || null,
      audio_sources: store.settings.audio_sources,
    };
    try {
      const saved = await invoke<Settings>("save_settings", { settings: next });
      store.settings = { ...saved, audio_sources: saved.audio_sources.map((source) => store.draftSources.get(source.id) ?? source) };
      notice("Preferences saved locally.");
      refreshDetectionState();
      refreshTranscriptionState();
      refreshDiarizationState();
      refreshSummaryState();
      refreshAppFlowyState();
    } catch (error) {
      notice(`Could not save preferences: ${String(error)}`, "error");
    }
  };
  wireAppFlowySettingsSection();
  wireSummarySection();
  wireFolderSection();
  wireExtensionSection();
  wireAudioSourcesSection();
  wireTranscriptionSection();
  wireSpeakersSection();
}

export { refreshAppState, refreshRecordingsFolderSection };
