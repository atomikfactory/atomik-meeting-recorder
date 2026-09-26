import { invoke, preview } from "../api";
import { notice } from "../ui/notice";
import { disabled, icon, store } from "../store";
import type { DetectionState } from "../types";
import { escapeAttr, escapeHtml, setTextIfChanged } from "../util/escape";
import { sourceSummary } from "../util/format";
import { recordingLabel, recentRecordings, refreshRecordingState, updateRecordingUI } from "./library";
import { renderTranscriptSearchResults } from "./search";

export function detectionLabel() {
  const labels: Record<string, string> = {
    disabled: "Automatic recording off", unsupported: "Automatic detection unavailable",
    waiting: "Waiting for a meeting", confirming: "Confirming meeting activity",
    awaiting_approval: "Record this meeting?",
    approval_received: "Preparing approved recording",
    recording: "Meeting recording active", grace: "Waiting for the meeting to end",
    suppressed: "Automatic recording paused", error: "Detection needs attention",
  };
  return labels[store.detectionState.phase] ?? "Checking meeting activity";
}

export function updateDetectionUI() {
  setTextIfChanged(document.querySelector("#detection-status"), detectionLabel());
  setTextIfChanged(document.querySelector("#detection-detail"), store.detectionState.detail);
  setTextIfChanged(document.querySelector("#detection-error"), store.detectionState.error ?? "");
  const seconds = store.detectionState.seconds_remaining;
  setTextIfChanged(document.querySelector("#detection-countdown"), seconds === null ? "" : `${seconds}s`);
  const panel = document.querySelector<HTMLElement>("#detection-panel");
  if (panel) panel.dataset.phase = store.detectionState.phase;
  const prompt = document.querySelector<HTMLElement>("#detection-prompt");
  if (prompt) prompt.hidden = store.detectionState.phase !== "awaiting_approval";
  document.querySelectorAll<HTMLButtonElement>("#detection-prompt button").forEach((button) => {
    button.disabled = preview || !store.backendReady || store.detectionActionInFlight || !store.detectionState.approval_id;
  });
  const help = store.detectionState.automation_inhibited
    ? "Automatic capture is disabled for this development test session. Your saved preference is unchanged."
    : store.detectionState.supported
      ? "Detects likely meetings locally and asks before each recording. Your end grace applies after approval. Manual controls are always available."
      : "Automatic detection is not available here. You can still start and stop recordings manually.";
  setTextIfChanged(document.querySelector("#detection-settings-help"), help);
}

export async function refreshDetectionState() {
  if (preview || !store.backendReady || document.hidden || store.detectionPollInFlight) return;
  store.detectionPollInFlight = true;
  try {
    const next = await invoke<DetectionState>("get_detection_state");
    const newRequest = next.phase === "awaiting_approval" && next.approval_id !== store.detectionState.approval_id;
    store.detectionState = next;
    if (newRequest) {
      document.querySelector<HTMLButtonElement>("[data-page='dashboard']")?.click();
      document.getElementById("detection-panel")?.scrollIntoView({ block: "center", behavior: "instant" });
    }
  } catch (error) {
    store.detectionState = { ...store.detectionState, phase: "error", seconds_remaining: null, error: `Could not check meeting detection: ${String(error)}` };
  } finally {
    store.detectionPollInFlight = false;
    updateDetectionUI();
  }
}

export function dashboardSectionHtml(): string {
  return `<section id="dashboard" class="page">
          <div class="hero">
            <div>
              <p class="kicker">ATOMIK MEETING RECORDER</p>
              <h1>Keep the conversation,<br /><em>not the busywork.</em></h1>
              <p class="lede">
                A calm, private place for your meeting notes and transcripts.
              </p>
            </div>
            <div class="hero-art"><span class="rings"></span>${icon}</div>
          </div>
          <div class="notice">
            <span>Info</span>
            <div>
              <strong>Your recordings stay on this computer.</strong><br /><small
                >Microphone and system audio remain on this device. Make sure everyone has consent before you start.${store.appState.tray_error ? ` ${escapeHtml(store.appState.tray_error)}` : ""}</small
              >
            </div>
          </div>
          <div class="action-row">
            <button id="record" class="primary" ${disabled() || !store.appState.recording_supported ? "disabled" : ""}>
              Start recording</button
            ><button id="stop-recording" class="primary stop-recording" hidden disabled>Stop recording</button
            ><button id="folder" class="secondary" ${disabled()}>
              Open recordings folder</button
            >
          </div>
          <div class="capabilities">
            <article>
              <span class="cap-dot"></span>
              <p>Audio</p>
              <b id="audio-summary">${escapeHtml(sourceSummary(store.settings.audio_sources))}</b
              ><small>Saved sources are used when you start a recording.</small>
            </article>
            <article>
              <span class="cap-dot"></span>
              <p>Processing</p>
              <b>Local transcription</b
              ><small
                >Whisper runs on this computer. Import a model in Settings.</small
              >
            </article>
            <article>
              <span class="cap-dot ${store.appflowyState.connected ? "ok" : "warn"}"></span>
              <p>AppFlowy</p>
              <b>${store.appflowyState.enabled ? (store.appflowyState.connected ? "Connected" : "Signed out") : "Off"}</b
              ><small>${escapeHtml(store.appflowyState.enabled ? (store.appflowyState.connected ? `Exporting to ${store.appflowyState.email ?? "your workspace"}.` : "Sign in under Settings to export.") : "Export is off; enable it in Settings to send meetings to AppFlowy.")}</small>
            </article>
          </div>
          <section class="recording-panel" aria-labelledby="recording-heading">
            <div><p class="kicker">CAPTURE</p><h2 id="recording-heading">Recording</h2></div>
            <div class="recording-readout"><b id="recording-timer" aria-live="off">00:00</b><span id="recording-status" role="status" aria-live="polite">${recordingLabel(store.recordingState.status)}</span></div>
            <p id="active-sources">${escapeHtml(sourceSummary(store.settings.audio_sources))}</p><div id="live-meters"></div>
            <p id="recording-error" class="recording-error" role="alert" aria-live="assertive"></p>
          </section>
          <section id="detection-panel" class="detection-panel" aria-labelledby="detection-status">
            <div class="detection-heading"><div><p class="kicker">MEETING DETECTION</p><h3 id="detection-status" role="status">${escapeHtml(detectionLabel())}</h3></div><span id="detection-countdown" aria-live="off"></span></div>
            <p id="detection-detail">${escapeHtml(store.detectionState.detail)}</p>
            <p id="detection-error" class="recording-error" role="alert"></p>
            <div id="detection-prompt" hidden><p>No audio is recorded until you approve. Make sure participants have consented.</p><div class="detection-actions"><button id="approve-meeting" class="primary">Record this meeting</button><button id="dismiss-meeting" class="secondary">Not this meeting</button></div></div>
          </section>
          <div id="transcript-search" class="transcript-search">
            <p class="kicker">SEARCH</p>
            <input id="transcript-search-input" type="search" aria-label="Search all transcripts and summaries" placeholder="Search every meeting's transcript and summary…" value="${escapeAttr(store.transcriptSearchQuery)}"/>
            <div id="transcript-search-results" role="region" aria-live="polite">${renderTranscriptSearchResults()}</div>
          </div>
          <div class="section-head">
            <div>
              <p class="kicker">RECENT</p>
              <h2>Meetings</h2>
            </div>
            <span>Local recordings</span>
          </div>
          <section class="transcription-panel"><p class="kicker">PROCESSING</p><h2>Local transcription</h2><p>Queued recordings process locally after capture. Recording can continue while this runs.</p><div id="transcription-jobs"><small>No recordings are queued.</small></div></section>
          <input id="history-search" aria-label="Search recent meetings" placeholder="Search recent meetings by title or date" value="${escapeAttr(store.historyQuery)}"/><div id="recent-recordings">${recentRecordings()}</div>
        </section>`;
}

export function wireDashboardSection() {
  document.querySelector("#record")?.addEventListener("click", async () => {
    if (store.recordingActionInFlight) return;
    if (!store.settings.audio_sources.length) {
      notice("Choose and save at least one audio source before recording.", "error");
      document.querySelector<HTMLButtonElement>("[data-page='settings']")?.click();
      return;
    }
    store.recordingEpoch++;
    store.recordingActionInFlight = true;
    updateRecordingUI();
    try {
      await invoke("start_recording");
    } catch (error) {
      notice(`Could not start recording: ${String(error)}`, "error");
    } finally {
      store.recordingActionInFlight = false;
      await refreshRecordingState(true);
      updateRecordingUI();
    }
  });
  document.querySelector("#stop-recording")?.addEventListener("click", async () => {
    if (store.recordingActionInFlight) return;
    store.recordingEpoch++;
    store.recordingActionInFlight = true;
    updateRecordingUI();
    try {
      await invoke("stop_recording");
    } catch (error) {
      notice(`Could not stop recording: ${String(error)}`, "error");
    } finally {
      store.recordingActionInFlight = false;
      await refreshRecordingState(true);
      updateRecordingUI();
    }
  });
  document.querySelector("#folder")?.addEventListener("click", () =>
    invoke("open_recordings_folder").catch((error) => notice(String(error), "error")),
  );
  for (const [id, command] of [["approve-meeting", "confirm_detected_recording"], ["dismiss-meeting", "dismiss_detected_recording"]]) {
    document.getElementById(id)?.addEventListener("click", async () => {
      const approvalId = store.detectionState.approval_id;
      if (store.detectionActionInFlight || !approvalId || store.detectionState.phase !== "awaiting_approval") return;
      store.detectionActionInFlight = true;
      updateDetectionUI();
      let actionError: string | null = null;
      try {
        await invoke(command, { approvalId });
      } catch (error) {
        actionError = `Could not update meeting request: ${String(error)}`;
        notice(actionError, "error");
      } finally {
        store.detectionActionInFlight = false;
        await refreshDetectionState();
        // A stale approval is an action error, not a reason to manufacture a
        // recording state. Keep it visible until the next detector sample.
        if (actionError) store.detectionState = { ...store.detectionState, error: actionError };
        await refreshRecordingState(true);
        updateDetectionUI();
      }
    });
  }
}

