// Per-meeting "Speakers" panel: naming a label ("You", "Speaker 1", …,
// "Remote") for one recording, remembering a voice for future meetings, and
// re-running recognition. Split out of library.ts to keep that file's
// existing per-row rendering focused; imports `updateRecordingUI` back from
// it the same way settings/summary.ts and settings/appflowy.ts already do
// (see their file headers) -- safe because it is only ever called from
// inside an event handler, well after both modules have finished loading.
import { invoke, preview } from "../api";
import { notice } from "../ui/notice";
import { store } from "../store";
import type { Recording, ReidentifyOutcome, SpeakerLabelInfo } from "../types";
import { escapeAttr, escapeHtml } from "../util/escape";
import { updateRecordingUI } from "./library";
import { refreshSummaryState as refreshSummaryStateAndUI } from "../settings/summary";

const REMOTE_LABEL_RE = /^Speaker \d+$/;

function busyKey(recordingId: string, label: string): string {
  return `${recordingId}\u0000${label}`;
}

/// Does this recording have any label worth showing a "Speakers" panel for
/// at all -- i.e. at least one microphone or output track, matching exactly
/// what the backend's `list_speaker_labels` would ever return something for
/// (see lib.rs). A recording with no usable tracks (e.g. a totally failed
/// capture) gets no panel rather than an always-empty one.
function hasSpeakerLabels(recording: Recording): boolean {
  return recording.tracks.some((t) => t.direction === "input" || t.direction === "output");
}

function speakerRowHtml(recordingId: string, label: SpeakerLabelInfo): string {
  const busy = store.speakerPanelBusy.has(busyKey(recordingId, label.label));
  if (label.label === "You") {
    return `<li class="speaker-row" data-speaker-row="${escapeAttr(label.label)}"><b>${escapeHtml(label.name || "You")}</b><small>You (set in Settings)</small></li>`;
  }
  const draft = store.speakerNameDrafts.get(recordingId)?.get(label.label) ?? label.name ?? "";
  const remember = store.speakerRememberDrafts.get(recordingId)?.get(label.label) ?? true;
  const canRemember = REMOTE_LABEL_RE.test(label.label);
  return `<li class="speaker-row" data-speaker-row="${escapeAttr(label.label)}">
    <b>${escapeHtml(label.label)}</b>${label.auto_identified ? `<span class="auto-badge" data-speaker-auto-badge title="Automatically identified from a remembered voice">auto</span>` : ""}
    <input type="text" maxlength="60" placeholder="Name this speaker" data-speaker-name-input="${escapeAttr(recordingId)}" data-speaker-label="${escapeAttr(label.label)}" value="${escapeAttr(draft)}" ${busy ? "disabled" : ""}/>
    ${canRemember ? `<label class="toggle small"><input type="checkbox" data-speaker-remember="${escapeAttr(recordingId)}" data-speaker-label="${escapeAttr(label.label)}" ${remember ? "checked" : ""} ${busy ? "disabled" : ""}/><span>Remember this voice for future meetings<small>A voice fingerprint is stored only on this device.</small></span></label>` : ""}
    <button type="button" class="secondary" data-speaker-save="${escapeAttr(recordingId)}" data-speaker-label="${escapeAttr(label.label)}" ${busy ? "disabled" : ""}>Save</button>
    ${label.name ? `<button type="button" class="secondary" data-speaker-clear="${escapeAttr(recordingId)}" data-speaker-label="${escapeAttr(label.label)}" ${busy ? "disabled" : ""}>Clear name</button>` : ""}
  </li>`;
}

export function speakerPanelHtml(recording: Recording): string {
  if (!hasSpeakerLabels(recording)) return "";
  const id = recording.recording_id;
  const open = store.speakerPanelOpen.has(id);
  const labels = store.speakerLabels.get(id);
  const error = store.speakerPanelError.get(id);
  const hint = store.speakerRegenerateHint.has(id);
  const reidentifyBusy = store.speakerReidentifyBusy.has(id);
  return `<details class="speakers-panel" data-speakers-panel="${escapeAttr(id)}" ${open ? "open" : ""}>
    <summary role="button" aria-expanded="${open}">Speakers</summary>
    ${!open ? "" : !labels ? `<p>Loading speakers…</p>` : `
    <ul>${labels.map((label) => speakerRowHtml(id, label)).join("")}</ul>
    ${error ? `<p role="alert" class="speaker-error" data-speaker-panel-error="${escapeAttr(id)}">${escapeHtml(error)}</p>` : ""}
    ${hint ? `<p class="speaker-hint"><span>Speaker names changed — Regenerate summary to use them</span><button type="button" class="secondary" data-speaker-regenerate-summary="${escapeAttr(id)}">Regenerate</button></p>` : ""}
    <div><button type="button" class="secondary" data-reidentify-speakers="${escapeAttr(id)}" ${reidentifyBusy ? "disabled" : ""}>Re-identify speakers</button></div>
    <small>AppFlowy pages already exported are never changed; new exports use these names.</small>
    `}
  </details>`;
}

async function loadSpeakerLabels(recordingId: string) {
  try {
    const previous = store.speakerLabels.get(recordingId);
    const labels = await invoke<SpeakerLabelInfo[]>("list_speaker_labels", { recordingId });
    const draftMap = store.speakerNameDrafts.get(recordingId) ?? new Map<string, string>();
    for (const label of labels) {
      // Only overwrite a draft the user has not diverged from what the
      // backend last reported for that label -- so a background refresh
      // (e.g. picking up an automatically assigned name) never clobbers text
      // someone is actively typing, per the polling-safety rule this panel
      // has to follow.
      const previousServerName = previous?.find((p) => p.label === label.label)?.name ?? "";
      const currentDraft = draftMap.get(label.label);
      if (currentDraft === undefined || currentDraft === previousServerName) {
        draftMap.set(label.label, label.name ?? "");
      }
    }
    store.speakerNameDrafts.set(recordingId, draftMap);
    store.speakerLabels.set(recordingId, labels);
    await refreshRegenerateHint(recordingId);
  } catch (error) {
    store.speakerPanelError.set(recordingId, `Could not load speakers: ${String(error)}`);
  } finally {
    updateRecordingUI();
  }
}

async function refreshRegenerateHint(recordingId: string) {
  try {
    const changed = await invoke<boolean>("speaker_names_changed_since_summary", { recordingId });
    if (changed) store.speakerRegenerateHint.add(recordingId);
    else store.speakerRegenerateHint.delete(recordingId);
  } catch {
    // Best-effort hint only; leave it as it was.
  }
}

async function saveSpeakerName(recordingId: string, label: string) {
  const key = busyKey(recordingId, label);
  if (store.speakerPanelBusy.has(key)) return;
  const name = (store.speakerNameDrafts.get(recordingId)?.get(label) ?? "").trim();
  if (!name) return;
  const remember = store.speakerRememberDrafts.get(recordingId)?.get(label) ?? true;
  store.speakerPanelBusy.add(key);
  updateRecordingUI();
  try {
    await invoke("set_speaker_name", { recordingId, label, name, remember });
    store.speakerPanelError.delete(recordingId);
  } catch (error) {
    // The name itself may already be saved even though "remember" failed
    // (e.g. not enough speech) -- see lib.rs's set_speaker_name. Show the
    // backend's refusal verbatim; the reload below still picks up whatever
    // did persist.
    store.speakerPanelError.set(recordingId, String(error));
  } finally {
    store.speakerPanelBusy.delete(key);
    await loadSpeakerLabels(recordingId);
  }
}

async function clearSpeakerName(recordingId: string, label: string) {
  const key = busyKey(recordingId, label);
  if (store.speakerPanelBusy.has(key)) return;
  store.speakerPanelBusy.add(key);
  updateRecordingUI();
  try {
    await invoke("delete_speaker_name", { recordingId, label });
    store.speakerNameDrafts.get(recordingId)?.set(label, "");
    store.speakerPanelError.delete(recordingId);
  } catch (error) {
    notice(`Could not clear speaker name: ${String(error)}`, "error");
  } finally {
    store.speakerPanelBusy.delete(key);
    await loadSpeakerLabels(recordingId);
  }
}

async function reidentifySpeakers(recordingId: string) {
  if (store.speakerReidentifyBusy.has(recordingId)) return;
  store.speakerReidentifyBusy.add(recordingId);
  store.speakerPanelError.delete(recordingId);
  updateRecordingUI();
  try {
    const applied = await invoke<ReidentifyOutcome[]>("reidentify_speakers", { recordingId });
    notice(applied.length ? `Identified ${applied.length} speaker${applied.length === 1 ? "" : "s"}.` : "No confident voice matches found.");
  } catch (error) {
    store.speakerPanelError.set(recordingId, String(error));
  } finally {
    store.speakerReidentifyBusy.delete(recordingId);
    await loadSpeakerLabels(recordingId);
  }
}

async function regenerateSummaryForSpeakerNames(recordingId: string) {
  try {
    await invoke("regenerate_summary", { recordingId });
    store.speakerRegenerateHint.delete(recordingId);
    await refreshSummaryStateAndUI();
  } catch (error) {
    notice(`Could not regenerate summary: ${String(error)}`, "error");
  } finally {
    updateRecordingUI();
  }
}

export function wireSpeakerPanels() {
  document.querySelectorAll<HTMLDetailsElement>("[data-speakers-panel]").forEach((details) => {
    details.ontoggle = () => {
      const id = details.dataset.speakersPanel!;
      if (details.open) {
        store.speakerPanelOpen.add(id);
        loadSpeakerLabels(id);
      } else {
        store.speakerPanelOpen.delete(id);
      }
    };
  });
  document.querySelectorAll<HTMLInputElement>("[data-speaker-name-input]").forEach((input) => {
    input.oninput = () => {
      const id = input.dataset.speakerNameInput!;
      const label = input.dataset.speakerLabel!;
      if (!store.speakerNameDrafts.has(id)) store.speakerNameDrafts.set(id, new Map());
      store.speakerNameDrafts.get(id)!.set(label, input.value);
    };
  });
  document.querySelectorAll<HTMLInputElement>("[data-speaker-remember]").forEach((input) => {
    input.onchange = () => {
      const id = input.dataset.speakerRemember!;
      const label = input.dataset.speakerLabel!;
      if (!store.speakerRememberDrafts.has(id)) store.speakerRememberDrafts.set(id, new Map());
      store.speakerRememberDrafts.get(id)!.set(label, input.checked);
    };
  });
  document.querySelectorAll<HTMLButtonElement>("[data-speaker-save]").forEach((button) => {
    button.onclick = () => saveSpeakerName(button.dataset.speakerSave!, button.dataset.speakerLabel!);
  });
  document.querySelectorAll<HTMLButtonElement>("[data-speaker-clear]").forEach((button) => {
    button.onclick = () => clearSpeakerName(button.dataset.speakerClear!, button.dataset.speakerLabel!);
  });
  document.querySelectorAll<HTMLButtonElement>("[data-reidentify-speakers]").forEach((button) => {
    button.onclick = () => reidentifySpeakers(button.dataset.reidentifySpeakers!);
  });
  document.querySelectorAll<HTMLButtonElement>("[data-speaker-regenerate-summary]").forEach((button) => {
    button.onclick = () => regenerateSummaryForSpeakerNames(button.dataset.speakerRegenerateSummary!);
  });
}

/// Called on the same cadence as the other background polls (see main.ts):
/// re-fetches labels (and the regenerate hint) for every currently expanded
/// panel, so an automatically assigned name or a "Speaker names changed"
/// hint appears without the user needing to collapse/reopen the panel.
export async function refreshOpenSpeakerPanels() {
  if (preview || !store.backendReady || document.hidden) return;
  for (const recordingId of store.speakerPanelOpen) {
    await loadSpeakerLabels(recordingId);
  }
}
