import { invoke, preview } from "../api";
import { notice } from "../ui/notice";
import { recordingBusy, store, summaryActionInFlight, appflowyActionInFlight, transcribeInFlight } from "../store";
import type { Recording, RecordingState } from "../types";
import { escapeAttr, escapeHtml, setTextIfChanged } from "../util/escape";
import { levelDb, recordingDuration, recordingSources, sourceSummary } from "../util/format";
import { refreshDeviceSection } from "../settings/audio";
import { transcriptionJob, transcriptionSummary, refreshTranscriptionState } from "../settings/transcription";
import { diarizationSummary, refreshDiarizationState } from "../settings/speakers";
import { summaryJob, summarySummary } from "../settings/summary";
import { appflowyJob, appflowySummary } from "../settings/appflowy";
import { audioStorageLine, compressionSummary } from "../settings/compression";

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

export function recentRecordings() {
  if (!store.recordingState.recent.length) return `<div class="empty"><div class="empty-wave">~~~</div><h3>Your recordings will appear here</h3><p>Audio and transcripts stay on this device. Import a local Whisper model in Settings to begin transcription. AppFlowy export is off by default; enable it in Settings.</p></div>`;
  const query = store.historyQuery.toLocaleLowerCase();
  const visible = store.recordingState.recent.filter((r) => !query || `${r.meeting_title ?? "Meeting"} ${new Date(r.started_at).toLocaleString()}`.toLocaleLowerCase().includes(query));
  return `<p class="history-scope">Searching the 25 most recent recordings. Older recordings stay in their original folders; use Open folder on a recording.</p><div class="recording-list">${visible.map((recording) => {
    const issues = recordingIssues(recording);
    const shortId = recording.recording_id.slice(0, 8);
    const started = new Date(recording.started_at).toLocaleString();
    const job = transcriptionJob(recording.recording_id); const diarization = store.diarizationState.jobs.find((item) => item.recording_id === recording.recording_id); const summary = summaryJob(recording.recording_id); const folder = `<button class="secondary" data-open-folder="${escapeAttr(recording.recording_id)}">Open folder</button>`; const action = job?.status === "completed" ? `<button class="secondary" data-open-transcript="${escapeAttr(recording.recording_id)}">Open transcript</button>` : job?.status === "failed" ? `<button class="secondary" data-retry-transcript="${escapeAttr(recording.recording_id)}">Retry transcription</button>` : job?.status === "waiting_model" ? `<button class="secondary" data-choose-model>Choose Whisper model${job.model_key ? `: ${escapeHtml(job.model_key)}` : ""}</button>` : !job && ["recorded", "interrupted", "failed"].includes(recording.status) && recording.tracks.some(t => t.frames_written > 0) ? `<button class="secondary" data-transcribe="${escapeAttr(recording.recording_id)}" ${transcribeInFlight.has(recording.recording_id) ? "disabled" : ""}>Transcribe</button>` : ""; const retry = diarization?.status === "failed" ? `<button class="secondary" data-retry-diarization="${escapeAttr(recording.recording_id)}">Retry speaker identification</button>` : "";
    const summaryBusy = summaryActionInFlight.has(recording.recording_id);
    const summaryActions = summary?.status === "completed" ? `<button class="secondary" data-open-summary="${escapeAttr(recording.recording_id)}" ${summaryBusy ? "disabled" : ""}>Open summary</button><button class="secondary" data-regenerate-summary="${escapeAttr(recording.recording_id)}" ${summaryBusy ? "disabled" : ""}>Regenerate summary</button>${summary.title && summary.title !== (recording.meeting_title ?? "Meeting") ? `<button class="secondary" data-use-suggested-title="${escapeAttr(recording.recording_id)}" data-title="${escapeAttr(summary.title)}" ${summaryBusy ? "disabled" : ""}>Use suggested title: ${escapeHtml(summary.title)}</button>` : ""}` : summary?.status === "failed" ? `<button class="secondary" data-retry-summary="${escapeAttr(recording.recording_id)}" ${summaryBusy ? "disabled" : ""}>Retry summary</button>` : "";
    const manage = `<button class="secondary" data-rename="${escapeAttr(recording.recording_id)}">Rename</button><button class="secondary" data-delete="${escapeAttr(recording.recording_id)}">Remove</button>${["completed", "failed", "waiting_model"].includes(job?.status ?? "") ? `<button class="secondary" data-restart="${escapeAttr(recording.recording_id)}">Restart transcription</button>` : ""}`;
    const storageLine = audioStorageLine(recording.recording_id); const compressionLine = compressionSummary(recording.recording_id);
    const appflowyBusy = appflowyActionInFlight.has(recording.recording_id);
    const appflowyJobFor = appflowyJob(recording.recording_id);
    const appflowyActions = !store.appflowyState.enabled ? "" : appflowyJobFor?.status === "completed" ? `<button class="secondary" data-appflowy-export-again="${escapeAttr(recording.recording_id)}" ${appflowyBusy ? "disabled" : ""}>Export to AppFlowy again</button>` : appflowyJobFor?.status === "failed" ? `<button class="secondary" data-appflowy-retry="${escapeAttr(recording.recording_id)}" ${appflowyBusy ? "disabled" : ""}>Retry AppFlowy export</button>` : !appflowyJobFor && store.appflowyState.connected ? `<button class="secondary" data-appflowy-export="${escapeAttr(recording.recording_id)}" ${appflowyBusy ? "disabled" : ""}>Export to AppFlowy</button>` : "";
    return `<article class="recording-row"><div><b>${escapeHtml(recording.meeting_title || "Meeting")}</b><small>${escapeHtml(started)} · ${recordingDuration(recording)}</small><small>${escapeHtml(recordingSources(recording))}</small><small>${escapeHtml(audioSummary(recording))}${storageLine ? ` · ${escapeHtml(storageLine)}` : ""}${compressionLine ? ` · ${escapeHtml(compressionLine)}` : ""} · ${escapeHtml(transcriptSummary(recording))} · ${escapeHtml(diarizationSummary(recording.recording_id))} · ${escapeHtml(summarySummary(recording.recording_id))} · ${escapeHtml(appflowySummary(recording.recording_id))}</small><small>Recording ${escapeHtml(shortId)}</small></div>${folder}${action}${retry}${summaryActions}${appflowyActions}${manage}${issues ? `<p>${escapeHtml(issues)}</p>` : ""}${diarization?.error ? `<p>${escapeHtml(diarization.error)}</p>` : ""}${summary?.error ? `<p>${escapeHtml(summary.error)}</p>` : ""}${appflowyJobFor?.error ? `<p>${escapeHtml(appflowyJobFor.error)}</p>` : ""}</article>`;
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
  document.querySelectorAll<HTMLButtonElement>("[data-open-transcript]").forEach(button => button.onclick = () => invoke("open_transcript", { recordingId: button.dataset.openTranscript }).catch(error => notice(String(error), "error")));
  document.querySelectorAll<HTMLButtonElement>("[data-open-folder]").forEach(button => button.onclick = () => invoke("open_recording_folder", { recordingId: button.dataset.openFolder }).catch(error => notice(String(error), "error")));
  document.querySelectorAll<HTMLButtonElement>("[data-transcribe]").forEach(button => button.onclick = async () => { const id = button.dataset.transcribe!; if (transcribeInFlight.has(id)) return; transcribeInFlight.add(id); updateRecordingUI(); try { await invoke("transcribe_recording", { recordingId: id }); await refreshTranscriptionState() } catch (error) { notice(`Could not queue transcription: ${String(error)}`, "error") } finally { transcribeInFlight.delete(id); updateRecordingUI() } });
  document.querySelectorAll<HTMLButtonElement>("[data-choose-model]").forEach(button => button.onclick = () => { document.querySelector<HTMLButtonElement>("[data-page='settings']")?.click(); setTimeout(() => document.querySelector<HTMLButtonElement>("#choose-model")?.focus(), 0) });
  const search = document.querySelector<HTMLInputElement>("#history-search"); if (search) search.oninput = event => { store.historyQuery = (event.target as HTMLInputElement).value; store.renderedRecentRecordings = null; updateRecordingUI(); };
  document.querySelectorAll<HTMLButtonElement>("[data-retry-transcript]").forEach(button => button.onclick = async () => { try { await invoke("retry_transcription", { recordingId: button.dataset.retryTranscript }); await refreshTranscriptionState(); } catch (error) { notice(`Could not retry transcription: ${String(error)}`, "error"); } });
  document.querySelectorAll<HTMLButtonElement>("[data-retry-diarization]").forEach(button => button.onclick = async () => { try { await invoke("retry_diarization", { recordingId: button.dataset.retryDiarization }); await refreshDiarizationState(); } catch (error) { notice(`Could not retry speaker identification: ${String(error)}`, "error"); } });
  document.querySelectorAll<HTMLButtonElement>("[data-open-summary]").forEach(button => button.onclick = () => invoke("open_summary", { recordingId: button.dataset.openSummary }).catch(error => notice(String(error), "error")));
  const runSummaryAction = async (id: string, command: string) => { if (summaryActionInFlight.has(id)) return; summaryActionInFlight.add(id); updateRecordingUI(); try { await invoke(command, { recordingId: id }); await refreshSummaryStateAndUI(); } catch (error) { notice(`Could not update summary: ${String(error)}`, "error"); } finally { summaryActionInFlight.delete(id); updateRecordingUI(); } };
  document.querySelectorAll<HTMLButtonElement>("[data-retry-summary]").forEach(button => button.onclick = () => runSummaryAction(button.dataset.retrySummary!, "retry_summary"));
  document.querySelectorAll<HTMLButtonElement>("[data-regenerate-summary]").forEach(button => button.onclick = () => runSummaryAction(button.dataset.regenerateSummary!, "regenerate_summary"));
  const runAppFlowyAction = async (id: string, command: string, args: Record<string, unknown> = {}) => { if (appflowyActionInFlight.has(id)) return; appflowyActionInFlight.add(id); updateRecordingUI(); try { await invoke(command, { recordingId: id, ...args }); await refreshAppFlowyStateAndUI(); } catch (error) { notice(`Could not update AppFlowy export: ${String(error)}`, "error"); } finally { appflowyActionInFlight.delete(id); updateRecordingUI(); } };
  document.querySelectorAll<HTMLButtonElement>("[data-appflowy-export]").forEach(button => button.onclick = () => runAppFlowyAction(button.dataset.appflowyExport!, "appflowy_export_now"));
  document.querySelectorAll<HTMLButtonElement>("[data-appflowy-retry]").forEach(button => button.onclick = () => runAppFlowyAction(button.dataset.appflowyRetry!, "appflowy_retry"));
  document.querySelectorAll<HTMLButtonElement>("[data-appflowy-export-again]").forEach(button => button.onclick = () => {
    const id = button.dataset.appflowyExportAgain!;
    if (!window.confirm("Export again? This creates a new page in AppFlowy alongside the existing one -- the earlier export is never modified.")) return;
    runAppFlowyAction(id, "appflowy_export_again", { confirm: true });
  });
  document.querySelectorAll<HTMLButtonElement>("[data-use-suggested-title]").forEach(button => button.onclick = async () => { const id = button.dataset.useSuggestedTitle!; const title = button.dataset.title!; try { await invoke("rename_recording", { recordingId: id, title }); await refreshRecordingState(true); } catch (error) { notice(`Could not use the suggested title: ${String(error)}`, "error"); } });
  wireManagementDialog();
}

// `updateRecordingUI` runs on every poll tick, so summary/AppFlowy action
// handlers re-fetch state through these thin re-imports rather than a static
// top-level import cycle back into those modules' own refresh functions --
// same behaviour as before the split, just named for clarity at the call site.
import { refreshSummaryState as refreshSummaryStateAndUI } from "../settings/summary";
import { refreshAppFlowyState as refreshAppFlowyStateAndUI } from "../settings/appflowy";

function wireManagementDialog() {
  const dialog = document.querySelector<HTMLDialogElement>("#management-dialog"); const open = (kind: "rename" | "delete" | "restart", id: string) => { if (store.managementBusy) return; store.management = { kind, id, title: "" }; const record = store.recordingState.recent.find(item => item.recording_id === id); const input = document.querySelector<HTMLInputElement>("#management-name")!; input.value = record?.meeting_title ?? "Meeting"; input.closest("label")!.toggleAttribute("hidden", kind !== "rename"); document.querySelector("#management-detail")!.textContent = kind === "delete" ? "Remove from this list? Audio remains recoverable on disk." : kind === "restart" ? "Archive previous transcript output and queue a new transcript with " + store.settings.whisper_model + "?" : "Change the meeting title."; document.querySelector("#management-error")!.textContent = ""; if (!dialog?.open) dialog?.showModal(); if (kind === "rename") input.focus() };
  document.querySelectorAll<HTMLButtonElement>("[data-rename]").forEach(button => button.onclick = () => open("rename", button.dataset.rename!)); document.querySelectorAll<HTMLButtonElement>("[data-delete]").forEach(button => button.onclick = () => open("delete", button.dataset.delete!)); document.querySelectorAll<HTMLButtonElement>("[data-restart]").forEach(button => button.onclick = () => open("restart", button.dataset.restart!));
  const cancel = document.querySelector<HTMLButtonElement>("#management-cancel")!; cancel.onclick = event => { event.preventDefault(); if (store.managementBusy) return; store.management = null; dialog?.close() }; if (dialog) dialog.oncancel = event => { if (store.managementBusy) event.preventDefault(); else store.management = null };
  document.querySelector<HTMLButtonElement>("#management-confirm")!.onclick = async event => { event.preventDefault(); if (!store.management || store.managementBusy) return; const task = store.management; store.managementBusy = true; const confirm = document.querySelector<HTMLButtonElement>("#management-confirm")!; confirm.disabled = true; cancel.disabled = true; try { if (task.kind === "rename") await invoke("rename_recording", { recordingId: task.id, title: document.querySelector<HTMLInputElement>("#management-name")!.value }); else await invoke(task.kind === "delete" ? "delete_recording" : "restart_transcription", { recordingId: task.id }); dialog?.close(); store.management = null; await refreshRecordingState(true); await refreshTranscriptionState() } catch (error) { document.querySelector("#management-error")!.textContent = String(error) } finally { store.managementBusy = false; confirm.disabled = false; cancel.disabled = false } };
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
