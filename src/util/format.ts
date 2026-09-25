import type { AudioSourceSelection, Recording } from "../types";

export function formatDuration(milliseconds: number) {
  const seconds = Math.max(0, Math.floor(milliseconds / 1000));
  return `${String(Math.floor(seconds / 60)).padStart(2, "0")}:${String(seconds % 60).padStart(2, "0")}`;
}

export function recordingDuration(recording: Recording) {
  return formatDuration(recording.status === "recording" ? Date.now() - recording.started_at : recording.duration_ms);
}

export function recordingSources(recording: Recording) {
  return recording.tracks.length ? recording.tracks.map((track) => track.name).join(", ") : "No source details available";
}

export function levelDb(value: number) {
  return Number.isFinite(value) ? Math.max(-96, Math.min(0, value)) : -96;
}

export function formatBytes(bytes: number) {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 MB";
  const mb = bytes / (1024 * 1024);
  return mb >= 1024 ? `${(mb / 1024).toFixed(2)} GB` : `${mb.toFixed(1)} MB`;
}

export function sourceInputId(id: string) {
  return `audio-source-${encodeURIComponent(id)}`;
}

export function sourceSummary(sources: AudioSourceSelection[]) {
  if (!sources.length) return "No sources selected";
  const names = sources.map((source) => source.name);
  if (names.length === 1) return names[0];
  if (names.length === 2) return names.join(", ");
  return `${names.slice(0, 2).join(", ")} +${names.length - 2}`;
}
