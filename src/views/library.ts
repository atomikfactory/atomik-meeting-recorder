import { invoke, preview } from "../api";
import { notice } from "../ui/notice";
import { recordingBusy, store, summaryActionInFlight, appflowyActionInFlight, atomikKbActionInFlight, transcribeInFlight } from "../store";
import type { LibraryFilter } from "../store";
import type { Recording, RecordingState } from "../types";
import { escapeAttr, escapeHtml, setTextIfChanged } from "../util/escape";
import { levelDb, recordingDuration, recordingSources, sourceSummary } from "../util/format";
import { refreshDeviceSection } from "../settings/audio";
import { transcriptionJob, refreshTranscriptionState } from "../settings/transcription";
import { diarizationSummary, refreshDiarizationState } from "../settings/speakers";
import { summaryJob, summarySummary } from "../settings/summary";
import { appflowyJob, appflowySummary, openAppFlowyUrl } from "../settings/appflowy";
import { atomikKbJob, atomikKbSummary } from "../settings/atomikKb";
import { audioStorageLine, compressionSummary } from "../settings/compression";
import { speakerPanelHtml, wireSpeakerPanels } from "./speakerPanel";

export function recordingIssues(recording: Recording) {
  if (recording.error) return recording.error;
  const trackErrors = recording.tracks.flatMap((track) => track.error ? [`${track.name}: ${track.error}`] : []);
  const dropped = recording.tracks.filter((track) => track.dropped_samples > 0).map((track) => `${track.name}: ${track.dropped_samples} samples dropped`);
  return [...trackErrors, ...dropped].join(" · ");
}

export function audioSummary(recording: Recording) {
  return recording.status === "recorded" ? "Audio saved" : recording.status === "interrupted" ? "Audio saved with interruptions" : "Audio failed";
}

export function transcriptSummary(recording: Recording) {
  const job = transcriptionJob(recording.recording_id);
  if (!job) return "Transcript not started";
  return job.status === "waiting_model" ? `Transcript needs model${job.model_key ? `: ${job.model_key}` : ""}` : job.status === "queued" ? "Transcript queued" : job.status === "transcribing" ? `Transcript working ${Math.round(job.progress)}%` : job.status === "completed" ? "Transcript completed" : "Transcript failed";
}

export function updateLiveMeters(container: HTMLElement) {
  if (store.recordingState.status !== "recording" || !store.recordingState.active) {
    if (container.childElementCount) container.replaceChildren();
    return;
  }
  const tracks = store.recordingState.active.tracks;
  const sourceIds = new Set(tracks.map((track) => track.source_id));
  container.querySelectorAll<HTMLElement>("[data-level-id]").forEach((row) => {
    if (!sourceIds.has(row.dataset.levelId ?? "")) row.remove();
  });
  const levels = new Map((store.recordingState.levels ?? []).map((level) => [level.source_id, level]));
  for (const track of tracks) {
    let row = container.querySelector<HTMLElement>(`[data-level-id="${CSS.escape(track.source_id)}"]`);
    if (!row) {
      row = document.createElement("div");
      row.className = "level-row";
      row.dataset.levelId = track.source_id;
      row.innerHTML = `<b></b><small class="level-state"></small><div class="meter" role="meter" aria-valuemin="-96" aria-valuemax="0"><i></i></div><small class="level-db"></small>`;
      container.append(row);
    }
    row.querySelector("b")!.textContent = track.name;
    row.querySelector(".meter")!.setAttribute("aria-label", `${track.name} level`);
    const level = levels.get(track.source_id);
    const valid = !!level && Number.isFinite(level.last_buffer_age_ms) && Number.isFinite(level.peak_dbfs) && Number.isFinite(level.rms_dbfs);
    const stale = valid && level!.last_buffer_age_ms! > 1500;
    const peak = !valid || stale ? -96 : levelDb(level!.peak_dbfs);
    const state = !valid ? "Waiting for audio" : stale ? "No recent audio" : level!.clipping ? "Clipping" : level!.signal_detected ? "Signal detected" : "Quiet";
    row.classList.toggle("is-clipping", valid && !!level?.clipping && !stale);
    row.querySelector(".level-state")!.textContent = `${track.direction === "output" ? "System audio / output" : "Mic / input"} · ${state}`;
    row.querySelector(".meter")!.setAttribute("aria-valuenow", String(peak));
    (row.querySelector(".meter i") as HTMLElement).style.transform = `scaleX(${(peak + 96) / 96})`;
    row.querySelector(".level-db")!.textContent = `${peak.toFixed(0)} dBFS`;
  }
}

export function recordingLabel(status: RecordingState["status"]) {
  return status === "recording" ? "Recording" : status === "processing" ? "Saving recording" : status === "failed" ? "Recording failed" : "Ready to record";
}

/// Is this meeting's AppFlowy export completed ("Synced to AppFlowy" filter)?
function isSyncedToAppFlowy(recording: Recording) {
  return appflowyJob(recording.recording_id)?.status === "completed";
}
/// Does this meeting have no completed summary ("No summary" filter)?
function hasNoCompletedSummary(recording: Recording) {
  return summaryJob(recording.recording_id)?.status !== "completed";
}
function passesLibraryFilter(recording: Recording): boolean {
  switch (store.libraryFilter) {
    case "synced": return isSyncedToAppFlowy(recording);
    case "not_synced": return !isSyncedToAppFlowy(recording);
    case "no_summary": return hasNoCompletedSummary(recording);
    default: return true;
  }
}
/// The list before the filter chips are applied, but after the free-text
/// history search box -- the same set the filter chips' counts are drawn
/// from, so a count always matches what choosing that chip will show.
function historySearched(): Recording[] {
  const query = store.historyQuery.toLocaleLowerCase();
  return store.recordingState.recent.filter((r) => !query || `${r.meeting_title ?? "Meeting"} ${new Date(r.started_at).toLocaleString()}`.toLocaleLowerCase().includes(query));
}
const LIBRARY_FILTERS: { key: LibraryFilter; label: string }[] = [
  { key: "all", label: "All" },
  { key: "synced", label: "Synced to AppFlowy" },
  { key: "not_synced", label: "Not synced" },
  { key: "no_summary", label: "No summary" },
];
function filterChipsHtml(searched: Recording[]) {
  const counts: Record<LibraryFilter, number> = {
    all: searched.length,
    synced: searched.filter(isSyncedToAppFlowy).length,
    not_synced: searched.filter((r) => !isSyncedToAppFlowy(r)).length,
    no_summary: searched.filter(hasNoCompletedSummary).length,
  };
  return `<div class="filter-chips" role="group" aria-label="Filter meetings">${LIBRARY_FILTERS.map(({ key, label }) =>
    `<button type="button" class="chip${store.libraryFilter === key ? " active" : ""}" aria-pressed="${store.libraryFilter === key}" data-filter="${key}">${escapeHtml(label)} (${counts[key]})</button>`,
  ).join("")}</div>`;
}
function selectionBarHtml(visibleIds: string[]) {
  const selected = visibleIds.filter((id) => store.selectedRecordingIds.has(id));
  if (!selected.length) return "";
  const busy = store.bulkActionBusy ? "disabled" : "";
  return `<div id="library-selection-bar" class="selection-bar" role="region" aria-live="polite">
    <span>${selected.length} selected</span>
    <button type="button" class="secondary" data-bulk-clear ${busy}>Clear selection</button>
    <button type="button" class="secondary" data-bulk-queue-transcription ${busy}>Queue transcription</button>
    <button type="button" class="secondary" data-bulk-queue-summary ${busy}>Queue summary</button>
    <button type="button" class="secondary" data-bulk-remove ${busy}>Remove from list</button>
    <button type="button" class="secondary" data-bulk-delete-disk ${busy}>Delete from disk</button>
  </div>`;
}

export function recentRecordings() {
  if (!store.recordingState.recent.length) return `<div class="empty"><div class="empty-wave">~~~</div><h3>Your recordings will appear here</h3><p>Audio and transcripts stay on this device. Import a local Whisper model in Settings to begin transcription. AppFlowy export is off by default; enable it in Settings.</p></div>`;
  // Selection persists across re-renders/filters/searches, but an id whose
  // recording has actually left the list (e.g. deleted) is pruned here.
  const knownIds = new Set(store.recordingState.recent.map((r) => r.recording_id));
  for (const id of store.selectedRecordingIds) if (!knownIds.has(id)) store.selectedRecordingIds.delete(id);
  const searched = historySearched();
  const visible = searched.filter(passesLibraryFilter);
  const visibleIds = visible.map((r) => r.recording_id);
  const allSelected = visibleIds.length > 0 && visibleIds.every((id) => store.selectedRecordingIds.has(id));
  const someSelected = visibleIds.some((id) => store.selectedRecordingIds.has(id));
  const selectAll = `<label class="select-row select-all"><input type="checkbox" id="library-select-all" ${allSelected ? "checked" : ""} data-indeterminate="${someSelected && !allSelected}" aria-label="Select all visible meetings"/><span>Select all (visible)</span></label>`;
  return `${filterChipsHtml(searched)}${selectionBarHtml(visibleIds)}<p class="history-scope">Searching the 25 most recent recordings. Older recordings stay in their original folders; use Open folder on a recording.</p>${visible.length ? selectAll : ""}<div class="recording-list">${visible.map((recording) => {
    const issues = recordingIssues(recording);
    const shortId = recording.recording_id.slice(0, 8);
    const started = new Date(recording.started_at).toLocaleString();
    const job = transcriptionJob(recording.recording_id);
    const diarization = store.diarizationState.jobs.find((item) => item.recording_id === recording.recording_id);
    const summary = summaryJob(recording.recording_id);
    const folder = `<button class="secondary" data-open-folder="${escapeAttr(recording.recording_id)}">Open folder</button>`;
    const action = job?.status === "completed"
      ? `<button class="secondary" data-open-transcript="${escapeAttr(recording.recording_id)}">Open transcript</button>`
      : job?.status === "failed"
        ? `<button class="secondary" data-retry-transcript="${escapeAttr(recording.recording_id)}">Retry transcription</button>`
        : job?.status === "waiting_model"
          ? `<button class="secondary" data-choose-model>Choose Whisper model${job.model_key ? `: ${escapeHtml(job.model_key)}` : ""}</button>`
          : !job && ["recorded", "interrupted", "failed"].includes(recording.status) && recording.tracks.some(t => t.frames_written > 0)
            ? `<button class="secondary" data-transcribe="${escapeAttr(recording.recording_id)}" ${transcribeInFlight.has(recording.recording_id) ? "disabled" : ""}>Transcribe</button>`
            : "";
    const retry = diarization?.status === "failed"
      ? `<button class="secondary" data-retry-diarization="${escapeAttr(recording.recording_id)}">Retry speaker identification</button>`
      : "";
    const summaryBusy = summaryActionInFlight.has(recording.recording_id);
    const suggestedTitle = summary?.status === "completed" && summary.title && summary.title !== (recording.meeting_title ?? "Meeting")
      ? `<p class="suggested-title"><span>Suggested title: <b>${escapeHtml(summary.title)}</b></span><button type="button" class="secondary" data-use-suggested-title="${escapeAttr(recording.recording_id)}" data-title="${escapeAttr(summary.title)}" aria-label="Use suggested title: ${escapeAttr(summary.title)}" ${summaryActionInFlight.has(recording.recording_id) ? "disabled" : ""}>Use</button></p>`
      : "";
    const summaryActions = summary?.status === "completed"
      ? `<button class="secondary" data-open-summary="${escapeAttr(recording.recording_id)}" ${summaryBusy ? "disabled" : ""}>Open summary</button><button class="secondary" data-regenerate-summary="${escapeAttr(recording.recording_id)}" ${summaryBusy ? "disabled" : ""}>Regenerate summary</button>`
      : summary?.status === "failed"
        ? `<button class="secondary" data-retry-summary="${escapeAttr(recording.recording_id)}" ${summaryBusy ? "disabled" : ""}>Retry summary</button>`
        : "";
    const manage = `<button class="secondary" data-rename="${escapeAttr(recording.recording_id)}">Rename</button><button class="secondary" data-delete="${escapeAttr(recording.recording_id)}">Remove</button>${["completed", "failed", "waiting_model"].includes(job?.status ?? "") ? `<button class="secondary" data-restart="${escapeAttr(recording.recording_id)}">Restart transcription</button>` : ""}`;
    const storageLine = audioStorageLine(recording.recording_id);
    const compressionLine = compressionSummary(recording.recording_id);
    const appflowyBusy = appflowyActionInFlight.has(recording.recording_id);
    const appflowyJobFor = appflowyJob(recording.recording_id);
    const appflowyActions = job?.status !== "completed"
      ? ""
      : !store.appflowyState.enabled || !store.appflowyState.connected
      ? `<button class="secondary" data-appflowy-setup>${store.appflowyState.enabled ? "Sign in to AppFlowy" : "Set up AppFlowy export"}</button>`
      : appflowyJobFor?.status === "queued" || appflowyJobFor?.status === "exporting"
      ? `<button class="secondary" disabled>${appflowyJobFor.status === "exporting" ? "Exporting to AppFlowy…" : "AppFlowy export queued"}</button>`
      : appflowyJobFor?.status === "completed"
        ? `${appflowyJobFor.page_url ? `<button class="secondary" data-open-appflowy-page="${escapeAttr(appflowyJobFor.page_url)}">Open in AppFlowy</button>` : ""}<button class="secondary" data-appflowy-export-again="${escapeAttr(recording.recording_id)}" ${appflowyBusy ? "disabled" : ""}>Export to AppFlowy again</button>`
        : appflowyJobFor?.status === "failed"
          ? `<button class="secondary" data-appflowy-retry="${escapeAttr(recording.recording_id)}" ${appflowyBusy ? "disabled" : ""}>Retry AppFlowy export</button>`
          : !appflowyJobFor && store.appflowyState.connected
            ? `<button class="secondary" data-appflowy-export="${escapeAttr(recording.recording_id)}" ${appflowyBusy ? "disabled" : ""}>Export to AppFlowy</button>`
            : "";
    const atomikBusy = atomikKbActionInFlight.has(recording.recording_id);
    const atomikJob = atomikKbJob(recording.recording_id);
    const atomikState = store.atomikKbState;
    const atomikActions = job?.status !== "completed"
      ? ""
      : !atomikState.enabled || !atomikState.has_token
      ? `<button class="secondary" data-atomik-kb-setup>${atomikState.enabled ? "Add Atomik KB token" : "Set up Atomik KB export"}</button>`
      : atomikJob?.status === "queued" || atomikJob?.status === "exporting"
      ? `<button class="secondary" disabled>${atomikJob.status === "exporting" ? "Exporting to Atomik KB…" : "Atomik KB export queued"}</button>`
      : atomikJob?.status === "completed"
        ? `<button class="secondary" data-atomik-kb-export-again="${escapeAttr(recording.recording_id)}" ${atomikBusy ? "disabled" : ""}>Export to Atomik KB again</button>`
        : atomikJob?.status === "failed"
          ? `<button class="secondary" data-atomik-kb-retry="${escapeAttr(recording.recording_id)}" ${atomikBusy ? "disabled" : ""}>Retry Atomik KB export</button>`
          : `<button class="secondary" data-atomik-kb-export="${escapeAttr(recording.recording_id)}" ${atomikBusy ? "disabled" : ""}>Export to Atomik KB</button>`;
    const checkbox = `<label class="select-row"><input type="checkbox" data-select="${escapeAttr(recording.recording_id)}" aria-label="Select ${escapeAttr(recording.meeting_title || "Meeting")}" ${store.selectedRecordingIds.has(recording.recording_id) ? "checked" : ""}/></label>`;
    const chip = (text: string, tone = "") => `<span class="chip-status${tone ? ` ${tone}` : ""}">${escapeHtml(text)}</span>`;
    const failedTone = (text: string) => /fail|needs model|error/i.test(text) ? "bad" : /complet|ready|exported|synced/i.test(text) ? "ok" : "";
    const statusChips = [audioSummary(recording), ...(storageLine ? [storageLine] : []), ...(compressionLine ? [compressionLine] : []), transcriptSummary(recording), diarizationSummary(recording.recording_id), summarySummary(recording.recording_id), appflowySummary(recording.recording_id), atomikKbSummary(recording.recording_id)]
      .filter((text): text is string => Boolean(text))
      .map((text) => chip(text, failedTone(text))).join("");
    const errors = [issues, diarization?.error, summary?.error, appflowyJobFor?.error, atomikJob?.error].filter(Boolean).map((text) => `<p>${escapeHtml(String(text))}</p>`).join("");
    return `<article class="recording-row">${checkbox}<div class="rec-main"><div class="rec-title"><b>${escapeHtml(recording.meeting_title || "Meeting")}</b><small>${escapeHtml(started)} · ${recordingDuration(recording)} · ${escapeHtml(recordingSources(recording))} · ID ${escapeHtml(shortId)}</small></div>${suggestedTitle}<div class="rec-status">${statusChips}</div></div><div class="rec-actions"><div class="rec-group">${action}${retry}</div><div class="rec-group">${summaryActions}</div><div class="rec-group">${appflowyActions}${atomikActions}</div></div><div class="rec-manage">${folder}${manage}</div>${errors}${speakerPanelHtml(recording)}</article>`;
  }).join("")}</div>`;
}

export function updateRecordingUI() {
  const active = store.recordingState.active;
  const live = store.recordingState.status === "recording" && Boolean(active);
  const status = document.querySelector<HTMLElement>("#recording-status");
  setTextIfChanged(status, recordingLabel(store.recordingState.status));
  if (status) status.dataset.active = live ? "true" : "false";
  const panel = document.querySelector<HTMLElement>(".recording-panel");
  if (panel) {
    panel.dataset.active = live ? "true" : "false";
    panel.classList.toggle("is-active", live);
  }
  document.body.classList.toggle("is-recording", store.recordingState.status === "recording" && Boolean(active));
  const topStatus = document.querySelector<HTMLElement>("#top-recording-status");
  setTextIfChanged(topStatus, recordingLabel(store.recordingState.status));
  setTextIfChanged(document.querySelector("#recording-heading"), store.recordingState.status === "recording" ? "Recording" : store.recordingState.status === "processing" ? "Saving recording" : "Ready to record");
  const timer = document.querySelector<HTMLElement>("#recording-timer");
  setTextIfChanged(timer, active ? recordingDuration(active) : "00:00");
  const sources = document.querySelector<HTMLElement>("#active-sources");
  setTextIfChanged(sources, active ? recordingSources(active) : sourceSummary(store.settings.audio_sources));
  const meters = document.querySelector<HTMLElement>("#live-meters"); if (meters) updateLiveMeters(meters);
  const error = document.querySelector<HTMLElement>("#recording-error");
  const warning = store.recordingState.status === "recording" && (store.recordingState.error ?? active?.error); setTextIfChanged(error, warning ? `Recording continues; audio may contain gaps. ${warning}` : (store.recordingState.error ?? active?.error ?? "")); if (error) error.dataset.warning = warning ? "true" : "false";
  const record = document.querySelector<HTMLButtonElement>("#record");
  if (record) {
    record.disabled = preview || !store.backendReady || !store.appState.recording_supported || store.recordingActionInFlight || recordingBusy();
    const stop = document.querySelector<HTMLButtonElement>("#stop-recording");
    if (stop) {
      stop.hidden = !active;
      stop.disabled = preview || !store.backendReady || store.recordingActionInFlight || !active;
    }
  }
  const nextNote = document.querySelector<HTMLElement>("#capture-next");
  setTextIfChanged(nextNote, recordingBusy() ? "Recording is active. Source changes are saved for the next recording." : "Select one or more sources for your next local recording.");
  const saveSources = document.querySelector<HTMLButtonElement>("#save-audio-sources");
  if (saveSources) saveSources.disabled = preview || !store.backendReady || store.audioSaveInFlight || recordingBusy();
  refreshDeviceSection();
  const meetings = document.querySelector<HTMLElement>("#recent-recordings");
  const nextRecent = recentRecordings();
  if (meetings && store.renderedRecentRecordings !== nextRecent) {
    const focused = document.activeElement as HTMLElement | null;
    const action = focused?.dataset.openTranscript ? "data-open-transcript" : focused?.dataset.retryTranscript ? "data-retry-transcript" : focused?.dataset.openFolder ? "data-open-folder" : focused?.dataset.transcribe ? "data-transcribe" : null;
    const focusedRecording = focused?.dataset.openTranscript ?? focused?.dataset.retryTranscript;
    meetings.innerHTML = nextRecent;
    store.renderedRecentRecordings = nextRecent;
    if (action && focusedRecording) Array.from(meetings.querySelectorAll<HTMLButtonElement>(`[${action}]`)).find(button => button.getAttribute(action) === focusedRecording)?.focus();
  }
  document.querySelectorAll<HTMLButtonElement>("[data-open-transcript]").forEach(button => button.onclick = () =>
    invoke("open_transcript", { recordingId: button.dataset.openTranscript }).catch(error => notice(String(error), "error")));
  document.querySelectorAll<HTMLButtonElement>("[data-open-folder]").forEach(button => button.onclick = () =>
    invoke("open_recording_folder", { recordingId: button.dataset.openFolder }).catch(error => notice(String(error), "error")));
  document.querySelectorAll<HTMLButtonElement>("[data-transcribe]").forEach(button => button.onclick = async () => {
    const id = button.dataset.transcribe!;
    if (transcribeInFlight.has(id)) return;
    transcribeInFlight.add(id);
    updateRecordingUI();
    try {
      await invoke("transcribe_recording", { recordingId: id });
      await refreshTranscriptionState();
    } catch (error) {
      notice(`Could not queue transcription: ${String(error)}`, "error");
    } finally {
      transcribeInFlight.delete(id);
      updateRecordingUI();
    }
  });
  document.querySelectorAll<HTMLButtonElement>("[data-choose-model]").forEach(button => button.onclick = () => {
    document.querySelector<HTMLButtonElement>("[data-page='settings']")?.click();
    setTimeout(() => document.querySelector<HTMLButtonElement>("#choose-model")?.focus(), 0);
  });
  const search = document.querySelector<HTMLInputElement>("#history-search");
  if (search) search.oninput = event => {
    store.historyQuery = (event.target as HTMLInputElement).value;
    store.renderedRecentRecordings = null;
    updateRecordingUI();
  };
  document.querySelectorAll<HTMLButtonElement>("[data-retry-transcript]").forEach(button => button.onclick = async () => {
    try {
      await invoke("retry_transcription", { recordingId: button.dataset.retryTranscript });
      await refreshTranscriptionState();
    } catch (error) {
      notice(`Could not retry transcription: ${String(error)}`, "error");
    }
  });
  document.querySelectorAll<HTMLButtonElement>("[data-retry-diarization]").forEach(button => button.onclick = async () => {
    try {
      await invoke("retry_diarization", { recordingId: button.dataset.retryDiarization });
      await refreshDiarizationState();
    } catch (error) {
      notice(`Could not retry speaker identification: ${String(error)}`, "error");
    }
  });
  document.querySelectorAll<HTMLButtonElement>("[data-open-summary]").forEach(button => button.onclick = () =>
    invoke("open_summary", { recordingId: button.dataset.openSummary }).catch(error => notice(String(error), "error")));
  const runSummaryAction = async (id: string, command: string) => {
    if (summaryActionInFlight.has(id)) return;
    summaryActionInFlight.add(id);
    updateRecordingUI();
    try {
      await invoke(command, { recordingId: id });
      await refreshSummaryStateAndUI();
    } catch (error) {
      notice(`Could not update summary: ${String(error)}`, "error");
    } finally {
      summaryActionInFlight.delete(id);
      updateRecordingUI();
    }
  };
  document.querySelectorAll<HTMLButtonElement>("[data-retry-summary]").forEach(button => button.onclick = () => runSummaryAction(button.dataset.retrySummary!, "retry_summary"));
  document.querySelectorAll<HTMLButtonElement>("[data-regenerate-summary]").forEach(button => button.onclick = () => runSummaryAction(button.dataset.regenerateSummary!, "regenerate_summary"));
  const runAppFlowyAction = async (id: string, command: string, args: Record<string, unknown> = {}) => {
    if (appflowyActionInFlight.has(id)) return;
    appflowyActionInFlight.add(id);
    updateRecordingUI();
    try {
      await invoke(command, { recordingId: id, ...args });
      await refreshAppFlowyStateAndUI();
    } catch (error) {
      notice(`Could not update AppFlowy export: ${String(error)}`, "error");
    } finally {
      appflowyActionInFlight.delete(id);
      updateRecordingUI();
    }
  };
  document.querySelectorAll<HTMLButtonElement>("[data-appflowy-export]").forEach(button => button.onclick = () => runAppFlowyAction(button.dataset.appflowyExport!, "appflowy_export_now"));
  document.querySelectorAll<HTMLButtonElement>("[data-appflowy-setup]").forEach(button => button.onclick = () => {
    document.querySelector<HTMLButtonElement>("[data-page='settings']")?.click();
    document.querySelector("#appflowy-settings-section")?.scrollIntoView({ block: "start" });
  });
  document.querySelectorAll<HTMLButtonElement>("[data-appflowy-retry]").forEach(button => button.onclick = () => runAppFlowyAction(button.dataset.appflowyRetry!, "appflowy_retry"));
  document.querySelectorAll<HTMLButtonElement>("[data-open-appflowy-page]").forEach(button => button.onclick = () => openAppFlowyUrl(button.dataset.openAppflowyPage!));
  document.querySelectorAll<HTMLButtonElement>("[data-appflowy-export-again]").forEach(button => button.onclick = () => {
    const id = button.dataset.appflowyExportAgain!;
    if (!window.confirm("Export again? This creates a new page in AppFlowy alongside the existing one -- the earlier export is never modified.")) return;
    runAppFlowyAction(id, "appflowy_export_again", { confirm: true });
  });
  const runAtomikKbAction = async (id: string, command: string, args: Record<string, unknown> = {}) => {
    if (atomikKbActionInFlight.has(id)) return;
    atomikKbActionInFlight.add(id);
    updateRecordingUI();
    try {
      await invoke(command, { recordingId: id, ...args });
      await refreshAtomikKbStateAndUI();
    } catch (error) {
      notice(`Could not update Atomik KB export: ${String(error)}`, "error");
    } finally {
      atomikKbActionInFlight.delete(id);
      updateRecordingUI();
    }
  };
  document.querySelectorAll<HTMLButtonElement>("[data-atomik-kb-export]").forEach(button => button.onclick = () => runAtomikKbAction(button.dataset.atomikKbExport!, "atomik_kb_export_now"));
  document.querySelectorAll<HTMLButtonElement>("[data-atomik-kb-setup]").forEach(button => button.onclick = () => {
    document.querySelector<HTMLButtonElement>("[data-page='settings']")?.click();
    document.querySelector("#atomik-kb-settings-section")?.scrollIntoView({ block: "start" });
  });
  document.querySelectorAll<HTMLButtonElement>("[data-atomik-kb-retry]").forEach(button => button.onclick = () => runAtomikKbAction(button.dataset.atomikKbRetry!, "atomik_kb_retry"));
  document.querySelectorAll<HTMLButtonElement>("[data-atomik-kb-export-again]").forEach(button => button.onclick = () => {
    const id = button.dataset.atomikKbExportAgain!;
    if (!window.confirm("Export again? This creates a new page in Atomik KB alongside the existing one -- the earlier export is never modified.")) return;
    runAtomikKbAction(id, "atomik_kb_export_again", { confirm: true });
  });
  document.querySelectorAll<HTMLButtonElement>("[data-use-suggested-title]").forEach(button => button.onclick = async () => {
    const id = button.dataset.useSuggestedTitle!;
    const title = button.dataset.title!;
    try {
      await invoke("rename_recording", { recordingId: id, title });
      await refreshRecordingState(true);
    } catch (error) {
      notice(`Could not use the suggested title: ${String(error)}`, "error");
    }
  });
  wireManagementDialog();
  wireLibrarySelection();
  wireBulkDialog();
  wireSpeakerPanels();
}

function selectedIdsInList(): string[] {
  const known = new Set(store.recordingState.recent.map((r) => r.recording_id));
  return Array.from(store.selectedRecordingIds).filter((id) => known.has(id));
}

function wireLibrarySelection() {
  document.querySelectorAll<HTMLInputElement>("[data-select]").forEach((input) => {
    input.onchange = () => {
      const id = input.dataset.select!;
      if (input.checked) store.selectedRecordingIds.add(id);
      else store.selectedRecordingIds.delete(id);
      updateRecordingUI();
    };
  });
  const selectAll = document.querySelector<HTMLInputElement>("#library-select-all");
  if (selectAll) {
    selectAll.indeterminate = selectAll.dataset.indeterminate === "true";
    selectAll.onchange = () => {
      const ids = Array.from(document.querySelectorAll<HTMLInputElement>("[data-select]")).map((i) => i.dataset.select!);
      if (selectAll.checked) ids.forEach((id) => store.selectedRecordingIds.add(id));
      else ids.forEach((id) => store.selectedRecordingIds.delete(id));
      updateRecordingUI();
    };
  }
  document.querySelectorAll<HTMLButtonElement>("[data-filter]").forEach((button) => {
    button.onclick = () => {
      store.libraryFilter = button.dataset.filter as LibraryFilter;
      store.renderedRecentRecordings = null;
      updateRecordingUI();
    };
  });
  const clear = document.querySelector<HTMLButtonElement>("[data-bulk-clear]");
  if (clear) clear.onclick = () => { store.selectedRecordingIds.clear(); updateRecordingUI(); };
  const queueTranscription = document.querySelector<HTMLButtonElement>("[data-bulk-queue-transcription]");
  if (queueTranscription) queueTranscription.onclick = runBulkQueueTranscription;
  const queueSummary = document.querySelector<HTMLButtonElement>("[data-bulk-queue-summary]");
  if (queueSummary) queueSummary.onclick = runBulkQueueSummary;
  const remove = document.querySelector<HTMLButtonElement>("[data-bulk-remove]");
  if (remove) remove.onclick = () => openBulkDialog("remove");
  const deleteFromDisk = document.querySelector<HTMLButtonElement>("[data-bulk-delete-disk]");
  if (deleteFromDisk) deleteFromDisk.onclick = () => openBulkDialog("delete");
}

async function runBulkQueueTranscription() {
  if (store.bulkActionBusy) return;
  const ids = selectedIdsInList();
  store.bulkActionBusy = true;
  updateRecordingUI();
  let queued = 0;
  let skipped = 0;
  for (const id of ids) {
    const recording = store.recordingState.recent.find((r) => r.recording_id === id);
    const job = transcriptionJob(id);
    try {
      if (job?.status === "failed") {
        await invoke("retry_transcription", { recordingId: id });
        queued++;
      } else if (!job && recording && ["recorded", "interrupted", "failed"].includes(recording.status) && recording.tracks.some((t) => t.frames_written > 0)) {
        await invoke("transcribe_recording", { recordingId: id });
        queued++;
      } else {
        skipped++;
      }
    } catch {
      skipped++;
    }
  }
  store.bulkActionBusy = false;
  store.lastBulkResult = `${queued} queued, ${skipped} skipped`;
  notice(store.lastBulkResult);
  await refreshTranscriptionState();
  await refreshRecordingState(true);
  updateRecordingUI();
}

async function runBulkQueueSummary() {
  if (store.bulkActionBusy) return;
  const ids = selectedIdsInList();
  store.bulkActionBusy = true;
  updateRecordingUI();
  let queued = 0;
  let skipped = 0;
  for (const id of ids) {
    const transcript = transcriptionJob(id);
    const summary = summaryJob(id);
    try {
      if (transcript?.status !== "completed" || summary?.status === "completed") {
        skipped++;
      } else if (summary?.status === "failed" || summary?.status === "waiting_ollama") {
        await invoke("retry_summary", { recordingId: id });
        queued++;
      } else if (!summary) {
        // No summary job at all yet -- e.g. the transcript finished before
        // summaries were enabled, or `auto_transcribe` was off -- so there
        // is nothing for retry/regenerate to act on. Enqueue a first job.
        await invoke("queue_summary", { recordingId: id });
        queued++;
      } else {
        skipped++;
      }
    } catch {
      skipped++;
    }
  }
  store.bulkActionBusy = false;
  store.lastBulkResult = `${queued} queued, ${skipped} skipped`;
  notice(store.lastBulkResult);
  await refreshSummaryStateAndUI();
  updateRecordingUI();
}

function openBulkDialog(kind: "remove" | "delete") {
  const ids = selectedIdsInList();
  if (!ids.length) return;
  store.bulkDialog = { kind, ids, mode: "recycle_bin" };
  store.bulkDialogError = "";
  renderBulkDialog();
  const dialog = document.querySelector<HTMLDialogElement>("#bulk-dialog");
  if (!dialog?.open) dialog?.showModal();
}

function bulkAudioBytes(ids: string[]) {
  const byId = new Map(store.audioStorage.recordings.map((r) => [r.recording_id, r.bytes]));
  return ids.reduce((total, id) => total + (byId.get(id) ?? 0), 0);
}
function formatBytes(bytes: number) {
  if (bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit++; }
  return `${value.toFixed(unit === 0 ? 0 : 1)} ${units[unit]}`;
}
function bulkTitles(ids: string[]) {
  const byId = new Map(store.recordingState.recent.map((r) => [r.recording_id, r.meeting_title ?? "Meeting"]));
  const titles = ids.map((id) => byId.get(id) ?? "Meeting").slice(0, 3);
  return titles.join(", ") + (ids.length > 3 ? `, and ${ids.length - 3} more` : "");
}

function renderBulkDialog() {
  const task = store.bulkDialog;
  const title = document.querySelector("#bulk-dialog-title");
  const detail = document.querySelector("#bulk-dialog-detail");
  const options = document.querySelector<HTMLElement>("#bulk-delete-options");
  const confirmLabel = document.querySelector<HTMLElement>("#bulk-confirm-label");
  const confirmTarget = document.querySelector("#bulk-confirm-target");
  const errorNode = document.querySelector("#bulk-dialog-error");
  const confirmButton = document.querySelector<HTMLButtonElement>("#bulk-dialog-confirm");
  const cancelButton = document.querySelector<HTMLButtonElement>("#bulk-dialog-cancel");
  if (!task || !title || !detail || !options || !confirmLabel || !errorNode || !confirmButton || !cancelButton) return;
  if (title) title.textContent = task.kind === "remove" ? "Remove from list" : "Delete from disk";
  const titles = bulkTitles(task.ids);
  if (task.kind === "remove") {
    detail.textContent = `Remove ${task.ids.length} meeting(s) from the list: ${titles}. Audio and transcripts stay on disk; nothing is deleted.`;
    options.hidden = true;
  } else {
    detail.textContent = `Permanently remove ${task.ids.length} meeting(s) from disk: ${titles}. Total audio: ${formatBytes(bulkAudioBytes(task.ids))}. AppFlowy pages already exported for these meetings are never touched or deleted.`;
    options.hidden = false;
    document.querySelectorAll<HTMLInputElement>('input[name="bulk-mode"]').forEach((radio) => {
      radio.checked = radio.value === task.mode;
      radio.onchange = () => { if (radio.checked) { store.bulkDialog = { ...task, mode: radio.value as "recycle_bin" | "permanent" }; renderBulkDialog(); } };
    });
    confirmLabel.hidden = task.mode !== "permanent";
    if (confirmTarget) confirmTarget.textContent = "DELETE";
  }
  errorNode.textContent = store.bulkDialogError;
  confirmButton.disabled = store.bulkDialogBusy || bulkConfirmDisabled();
  cancelButton.disabled = store.bulkDialogBusy;
}

function bulkConfirmDisabled(): boolean {
  const task = store.bulkDialog;
  if (!task) return true;
  if (task.kind === "remove" || task.mode === "recycle_bin") return false;
  const input = (document.querySelector<HTMLInputElement>("#bulk-confirm-input")?.value ?? "").trim();
  return input !== "DELETE" && input !== String(task.ids.length);
}

function wireBulkDialog() {
  const dialog = document.querySelector<HTMLDialogElement>("#bulk-dialog");
  const confirmInput = document.querySelector<HTMLInputElement>("#bulk-confirm-input");
  if (confirmInput) confirmInput.oninput = () => renderBulkDialog();
  const cancel = document.querySelector<HTMLButtonElement>("#bulk-dialog-cancel");
  if (cancel) cancel.onclick = (event) => {
    event.preventDefault();
    if (store.bulkDialogBusy) return;
    store.bulkDialog = null;
    dialog?.close();
  };
  if (dialog) dialog.oncancel = (event) => {
    if (store.bulkDialogBusy) event.preventDefault();
    else store.bulkDialog = null;
  };
  const confirm = document.querySelector<HTMLButtonElement>("#bulk-dialog-confirm");
  if (confirm) confirm.onclick = async (event) => {
    event.preventDefault();
    const task = store.bulkDialog;
    if (!task || store.bulkDialogBusy || bulkConfirmDisabled()) return;
    store.bulkDialogBusy = true;
    renderBulkDialog();
    try {
      if (task.kind === "remove") {
        let removed = 0;
        let failed = 0;
        for (const id of task.ids) {
          try { await invoke("delete_recording", { recordingId: id }); removed++; }
          catch { failed++; }
        }
        store.lastBulkResult = `${removed} removed from the list${failed ? `, ${failed} failed` : ""}`;
      } else {
        const summary = await invoke<{ results: { recording_id: string; deleted: boolean; skipped_reason: string | null }[] }>(
          "delete_recordings_from_disk",
          { recordingIds: task.ids, mode: task.mode },
        );
        const deleted = summary.results.filter((r) => r.deleted).length;
        const skippedReasons = summary.results.filter((r) => !r.deleted).map((r) => r.skipped_reason).filter(Boolean);
        store.lastBulkResult = `Deleted ${deleted}, skipped ${skippedReasons.length}${skippedReasons.length ? `: ${skippedReasons.join("; ")}` : ""}`;
      }
      notice(store.lastBulkResult!);
      store.selectedRecordingIds.clear();
      store.bulkDialog = null;
      dialog?.close();
      await refreshRecordingState(true);
    } catch (error) {
      store.bulkDialogError = String(error);
      renderBulkDialog();
    } finally {
      store.bulkDialogBusy = false;
      updateRecordingUI();
    }
  };
}

// `updateRecordingUI` runs on every poll tick, so summary/AppFlowy action
// handlers re-fetch state through these thin re-imports rather than a static
// top-level import cycle back into those modules' own refresh functions --
// same behaviour as before the split, just named for clarity at the call site.
import { refreshSummaryState as refreshSummaryStateAndUI } from "../settings/summary";
import { refreshAppFlowyState as refreshAppFlowyStateAndUI } from "../settings/appflowy";
import { refreshAtomikKbState as refreshAtomikKbStateAndUI } from "../settings/atomikKb";

function wireManagementDialog() {
  const dialog = document.querySelector<HTMLDialogElement>("#management-dialog");
  const open = (kind: "rename" | "delete" | "restart", id: string) => {
    if (store.managementBusy) return;
    store.management = { kind, id, title: "" };
    const record = store.recordingState.recent.find(item => item.recording_id === id);
    const input = document.querySelector<HTMLInputElement>("#management-name")!;
    input.value = record?.meeting_title ?? "Meeting";
    input.closest("label")!.toggleAttribute("hidden", kind !== "rename");
    document.querySelector("#management-detail")!.textContent = kind === "delete"
      ? "Remove from this list? Audio remains recoverable on disk."
      : kind === "restart"
        ? "Archive previous transcript output and queue a new transcript with " + store.settings.whisper_model + "?"
        : "Change the meeting title.";
    document.querySelector("#management-error")!.textContent = "";
    if (!dialog?.open) dialog?.showModal();
    if (kind === "rename") input.focus();
  };
  document.querySelectorAll<HTMLButtonElement>("[data-rename]").forEach(button => button.onclick = () => open("rename", button.dataset.rename!));
  document.querySelectorAll<HTMLButtonElement>("[data-delete]").forEach(button => button.onclick = () => open("delete", button.dataset.delete!));
  document.querySelectorAll<HTMLButtonElement>("[data-restart]").forEach(button => button.onclick = () => open("restart", button.dataset.restart!));
  const cancel = document.querySelector<HTMLButtonElement>("#management-cancel")!;
  cancel.onclick = event => {
    event.preventDefault();
    if (store.managementBusy) return;
    store.management = null;
    dialog?.close();
  };
  if (dialog) dialog.oncancel = event => {
    if (store.managementBusy) event.preventDefault();
    else store.management = null;
  };
  document.querySelector<HTMLButtonElement>("#management-confirm")!.onclick = async event => {
    event.preventDefault();
    if (!store.management || store.managementBusy) return;
    const task = store.management;
    store.managementBusy = true;
    const confirm = document.querySelector<HTMLButtonElement>("#management-confirm")!;
    confirm.disabled = true;
    cancel.disabled = true;
    try {
      if (task.kind === "rename") {
        await invoke("rename_recording", { recordingId: task.id, title: document.querySelector<HTMLInputElement>("#management-name")!.value });
      } else {
        await invoke(task.kind === "delete" ? "delete_recording" : "restart_transcription", { recordingId: task.id });
      }
      dialog?.close();
      store.management = null;
      await refreshRecordingState(true);
      await refreshTranscriptionState();
    } catch (error) {
      document.querySelector("#management-error")!.textContent = String(error);
    } finally {
      store.managementBusy = false;
      confirm.disabled = false;
      cancel.disabled = false;
    }
  };
}

export async function refreshRecordingState(force = false) {
  if (preview || !store.backendReady || document.hidden) return;
  if (!force && store.recordingState.status !== "recording" && Date.now() - store.lastIdleRecordingPoll < 1000) return;
  if (store.recordingState.status !== "recording") store.lastIdleRecordingPoll = Date.now();
  if (store.recordingActionInFlight && !force) return;
  if (store.recordingPollPromise) {
    await store.recordingPollPromise;
    if (!force) return;
  }
  const epoch = store.recordingEpoch;
  const request = (async () => {
    try {
      const next = await invoke<RecordingState>("get_recording_state");
      if (epoch !== store.recordingEpoch || store.recordingActionInFlight) return;
      store.recordingState = next;
      updateRecordingUI();
    } catch (error) {
      if (epoch !== store.recordingEpoch || store.recordingActionInFlight) return;
      store.recordingState = { ...store.recordingState, error: `Could not check recording status: ${String(error)}` };
      updateRecordingUI();
    }
  })();
  store.recordingPollPromise = request;
  try { await request; } finally { if (store.recordingPollPromise === request) store.recordingPollPromise = null; }
}
