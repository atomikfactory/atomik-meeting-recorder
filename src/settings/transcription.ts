import { invoke, preview } from "../api";
import { notice } from "../ui/notice";
import { store } from "../store";
import type { Model, TranscriptionState } from "../types";
import { escapeHtml, setTextIfChanged } from "../util/escape";
import { updateRecordingUI } from "../views/library";

export function transcriptionJob(id: string) {
  return store.transcriptionState.jobs.find((job) => job.recording_id === id);
}

export async function refreshTranscriptionState() {
  if (preview || !store.backendReady || document.hidden || store.transcriptionPollInFlight) return;
  store.transcriptionPollInFlight = true;
  try {
    store.transcriptionState = await invoke<TranscriptionState>("get_transcription_state");
  } catch (error) {
    store.transcriptionState = { ...store.transcriptionState, error: `Could not check local transcription: ${String(error)}` };
  } finally {
    store.transcriptionPollInFlight = false;
    updateTranscriptionUI();
  }
}

export function updateTranscriptionUI() {
  setTextIfChanged(document.querySelector("#model-status"), store.transcriptionState.model.available
    ? `Ready: ${store.transcriptionState.model.key}` : (store.transcriptionState.model.error ?? `Import the ${store.transcriptionState.model.key || "selected"} model to begin`));
  setTextIfChanged(document.querySelector("#backend-status"), `Backend: ${store.transcriptionState.backend}`);
  const imported = store.transcriptionState.imported_models ?? [];
  document.querySelectorAll<HTMLOptionElement>('select[name="whisper_model"] option').forEach((option) => {
    const size = option.value as Model;
    option.textContent = `${size}${imported.includes(size) ? "" : " (not imported)"}`;
  });
  setTextIfChanged(document.querySelector("#model-select-warning"), imported.includes(store.settings.whisper_model)
    ? "" : `${store.settings.whisper_model} is not imported yet; jobs will wait until you import it or pick an imported size.`);
  const jobs = document.querySelector<HTMLElement>("#transcription-jobs");
  const content = `${store.transcriptionState.inhibited ? "<p>Local transcription is temporarily inhibited.</p>" : ""}${store.transcriptionState.error ? `<p class="recording-error" role="alert">${escapeHtml(store.transcriptionState.error)}</p>` : ""}` +
    (store.transcriptionState.jobs.slice(0, 25).map(job => `<div><b>${escapeHtml(job.status.replaceAll("_", " "))}</b> <small>Recording ${escapeHtml(job.recording_id.slice(0, 8))}</small>${job.status === "transcribing" ? ` <span>${Math.round(job.progress)}%</span>` : ""}${job.error ? `<small>${escapeHtml(job.error)}</small>` : ""}</div>`).join("") || "<small>No recordings are queued.</small>");
  if (jobs && jobs.innerHTML !== content) jobs.innerHTML = content;
  updateRecordingUI();
}

export function wireTranscriptionSection() {
  document.querySelector("#choose-model")?.addEventListener("click", async () => {
    const button = document.querySelector<HTMLButtonElement>("#choose-model");
    if (button) button.disabled = true;
    try {
      const result = await invoke<{ key: string; switched_to?: string | null } | null>("import_whisper_model");
      if (result) {
        if (result.switched_to) {
          store.settings = { ...store.settings, whisper_model: result.switched_to as Model };
          const select = document.querySelector<HTMLSelectElement>('select[name="whisper_model"]');
          if (select) select.value = result.switched_to;
        }
        notice(result.switched_to ? `Imported ${result.key}. Switched your selected model to ${result.key} so it's used right away.` : result.key === store.settings.whisper_model ? `Imported ${result.key}. Ready for local transcription.` : `Imported ${result.key}. Select ${result.key} above and save preferences to use it for new jobs.`);
      }
      await refreshTranscriptionState();
    }
    catch (error) { notice(`Could not choose local model: ${String(error)}`, "error"); }
    finally { if (button) button.disabled = preview || !store.backendReady; }
  });
}
