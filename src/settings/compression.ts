import { invoke, preview } from "../api";
import { store } from "../store";
import type { AudioStorageSummary, CompressionState } from "../types";
import { formatBytes } from "../util/format";
import { updateRecordingUI } from "../views/library";

export function audioStorageEntry(recordingId: string) {
  return store.audioStorage.recordings.find((r) => r.recording_id === recordingId);
}

export function audioStorageLine(recordingId: string) {
  const entry = audioStorageEntry(recordingId);
  if (!entry || entry.format === "none" || entry.bytes <= 0) return "";
  const saved = entry.original_bytes > entry.bytes && entry.original_bytes > 0 ? Math.round((1 - entry.bytes / entry.original_bytes) * 100) : 0;
  const label = entry.format === "flac" ? "FLAC" : entry.format === "wav" ? "WAV" : "Mixed";
  return saved > 0 ? `${label} · ${formatBytes(entry.bytes)} (saved ${saved}%)` : `${label} · ${formatBytes(entry.bytes)}`;
}

export function compressionSummary(recordingId: string) {
  const job = store.compressionState.jobs.find((item) => item.recording_id === recordingId);
  if (job?.status === "failed") return "Audio compression failed";
  if (job?.status === "compressing") return "Compressing audio";
  return "";
}

export function audioStorageSummaryText() {
  if (!store.audioStorage.total_bytes) return "Audio on disk: not yet measured.";
  const saved = store.audioStorage.total_original_bytes > store.audioStorage.total_bytes && store.audioStorage.total_original_bytes > 0
    ? Math.round((1 - store.audioStorage.total_bytes / store.audioStorage.total_original_bytes) * 100)
    : 0;
  return saved > 0
    ? `Audio on disk: ${formatBytes(store.audioStorage.total_bytes)} (was ${formatBytes(store.audioStorage.total_original_bytes)}, saved ${saved}%).`
    : `Audio on disk: ${formatBytes(store.audioStorage.total_bytes)}.`;
}

export async function refreshCompressionState() {
  if (preview || !store.backendReady || document.hidden || store.compressionPoll) return;
  store.compressionPoll = true;
  try {
    store.compressionState = await invoke<CompressionState>("get_compression_state");
  } catch (error) {
    store.compressionState = { ...store.compressionState, error: `Could not check audio compression: ${String(error)}` };
  } finally {
    store.compressionPoll = false;
    updateRecordingUI();
  }
}

export async function refreshAudioStorage() {
  if (preview || !store.backendReady || document.hidden || store.audioStoragePoll) return;
  store.audioStoragePoll = true;
  try {
    store.audioStorage = await invoke<AudioStorageSummary>("audio_storage_summary");
  } catch {
    /* best-effort only */
  } finally {
    store.audioStoragePoll = false;
    updateRecordingUI();
  }
}
