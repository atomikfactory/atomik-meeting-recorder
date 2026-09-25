import { invoke, preview } from "../api";
import { notice } from "../ui/notice";
import { store } from "../store";
import type { DiarizationState } from "../types";
import { updateRecordingUI } from "../views/library";

export function diarizationSummary(recordingId: string) {
  const job = store.diarizationState.jobs.find((item) => item.recording_id === recordingId);
  if (!job) return store.diarizationState.enabled ? "Waiting for speaker identification" : "Speaker identification off";
  if (job.status === "completed") return job.speaker_count === null ? "Speaker labels completed" : `${job.speaker_count} speakers estimated`;
  if (job.status === "diarizing") return `Identifying speakers ${Math.round(job.progress)}%`;
  return job.status.replaceAll("_", " ");
}

export async function refreshDiarizationState() {
  if (preview || !store.backendReady || document.hidden || store.diarizationPoll) return;
  store.diarizationPoll = true;
  try {
    store.diarizationState = await invoke<DiarizationState>("get_diarization_state");
  } catch (error) {
    store.diarizationState = { ...store.diarizationState, error: `Could not check speaker identification: ${String(error)}` };
  } finally {
    store.diarizationPoll = false;
    updateDiarizationUI();
  }
}

export function updateDiarizationUI() {
  const node = document.querySelector<HTMLElement>("#diarization-status");
  const segmentation = store.diarizationState.models.find((m) => m.kind === "segmentation");
  const embedding = store.diarizationState.models.find((m) => m.kind === "embedding");
  const modelError = [segmentation, embedding].flatMap((m) => (m?.error ? [m.error] : [])).join(" ");
  if (node) node.textContent = store.diarizationState.error ?? (store.diarizationState.inhibited ? "Speaker identification is temporarily inhibited." : modelError || (segmentation?.available && embedding?.available ? "Local speaker identification ready." : "Choose both local models to identify speakers. Transcription remains available."));
  updateRecordingUI();
}

export function wireSpeakersSection() {
  document.querySelectorAll<HTMLButtonElement>("[data-diarization-kind]").forEach((button) => button.onclick = async () => {
    button.disabled = true;
    try {
      const result = await invoke<{ key: string } | null>("import_diarization_model", { kind: button.dataset.diarizationKind });
      if (result) notice(`Imported ${result.key} locally.`);
      await refreshDiarizationState();
    } catch (error) {
      notice(`Could not choose local speaker model: ${String(error)}`, "error");
    } finally {
      button.disabled = preview || !store.backendReady;
    }
  });
}
