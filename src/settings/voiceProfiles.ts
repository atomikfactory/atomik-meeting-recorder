import { invoke, preview } from "../api";
import { notice } from "../ui/notice";
import { disabled, store } from "../store";
import type { VoiceProfileInfo, VoiceProfileSample } from "../types";
import { escapeAttr, escapeHtml } from "../util/escape";
import { onClipPlaybackChange, playClip, playingClipKey, stopClip } from "../util/clipPlayer";

onClipPlaybackChange(() => updateVoiceProfilesUI());

const sampleLoading = new Set<string>();
const sampleClipErrors = new Map<string, string>();

/// Loads the saved voice profile list (name, seconds of speech, last
/// updated) for the Settings → "Voice profiles" section. Best-effort: a
/// failure is shown as a status line rather than blocking the rest of
/// Settings from rendering.
export async function refreshVoiceProfiles() {
  if (preview || !store.backendReady) return;
  try {
    store.voiceProfiles = await invoke<VoiceProfileInfo[]>("list_voice_profiles");
    store.voiceProfilesError = null;
  } catch (error) {
    store.voiceProfilesError = `Could not load voice profiles: ${String(error)}`;
  } finally {
    store.voiceProfilesLoaded = true;
    updateVoiceProfilesUI();
  }
}

function formatSeconds(seconds: number): string {
  return seconds >= 60 ? `${(seconds / 60).toFixed(1)} min` : `${seconds.toFixed(1)}s`;
}

function formatDate(seconds: number): string {
  return new Date(seconds * 1000).toLocaleDateString();
}

function sampleRowHtml(sample: VoiceProfileSample, busy: boolean): string {
  const key = sample.id;
  const playing = playingClipKey() === key;
  const loading = sampleLoading.has(key);
  const error = sampleClipErrors.get(key);
  const title = sample.source_title?.trim() || "Unknown meeting";
  return `<li data-voice-sample="${escapeAttr(sample.id)}">
    <span>${escapeHtml(formatSeconds(sample.seconds))} · ${escapeHtml(title)} · ${escapeHtml(formatDate(sample.created_at))}</span>
    <button type="button" class="secondary" data-voice-sample-play="${escapeAttr(sample.id)}" aria-label="${playing ? "Stop" : "Play"} clip from ${escapeAttr(title)}" aria-pressed="${playing}" ${loading ? "disabled" : ""}>${loading ? "Loading…" : playing ? "Stop" : "Play"}</button>
    <button type="button" class="secondary" data-voice-sample-delete="${escapeAttr(sample.id)}" aria-label="Delete clip from ${escapeAttr(title)}" ${disabled() || busy ? "disabled" : ""}>Delete</button>
    ${error ? `<small role="alert" data-voice-sample-error="${escapeAttr(sample.id)}">${escapeHtml(error)}</small>` : ""}
  </li>`;
}

function profileDetailsHtml(profile: VoiceProfileInfo, busy: boolean): string {
  const samples = store.voiceProfileSamples.get(profile.id);
  const loadError = store.voiceProfileSampleErrors.get(profile.id);
  const others = store.voiceProfiles.filter((p) => p.id !== profile.id);
  const mergeTarget = store.voiceProfileMergeDrafts.get(profile.id) ?? "";
  const mergeError = store.voiceProfileMergeErrors.get(profile.id);
  const list = loadError ? `<p role="alert" data-voice-profile-samples-error="${escapeAttr(profile.id)}">${escapeHtml(loadError)}</p>`
    : !samples ? `<p>Loading clips…</p>`
      : samples.length === 0 ? `<p data-voice-profile-no-samples="${escapeAttr(profile.id)}">No clips stored for this person.</p>`
        : `<ul class="voice-samples" aria-label="Clips for ${escapeAttr(profile.name)}">${samples.map((s) => sampleRowHtml(s, busy)).join("")}</ul>`;
  const merge = others.length === 0 ? "" : `<div class="voice-merge">
      <label>Merge ${escapeHtml(profile.name)} into
        <select data-voice-profile-merge-target="${escapeAttr(profile.id)}" ${disabled() || busy ? "disabled" : ""}>
          <option value="">Choose a person…</option>
          ${others.map((o) => `<option value="${escapeAttr(o.id)}" ${o.id === mergeTarget ? "selected" : ""}>${escapeHtml(o.name)}</option>`).join("")}
        </select>
      </label>
      <button type="button" class="secondary" data-voice-profile-merge="${escapeAttr(profile.id)}" ${disabled() || busy || !mergeTarget ? "disabled" : ""}>Merge into…</button>
      ${mergeError ? `<small role="alert" data-voice-profile-merge-error="${escapeAttr(profile.id)}">${escapeHtml(mergeError)}</small>` : ""}
    </div>`;
  return `<div class="voice-profile-details" data-voice-profile-details="${escapeAttr(profile.id)}">${list}${merge}</div>`;
}

function voiceProfileRowHtml(profile: VoiceProfileInfo): string {
  const busy = store.voiceProfilesBusy.has(profile.id);
  const draft = store.voiceProfileRenameDrafts.get(profile.id) ?? profile.name;
  const updated = new Date(profile.updated_at * 1000).toLocaleString();
  const expanded = store.voiceProfileExpanded.has(profile.id);
  const clips = `${profile.sample_count} clip${profile.sample_count === 1 ? "" : "s"}`;
  return `<li data-voice-profile="${escapeAttr(profile.id)}">
    <input type="text" aria-label="Name of voice profile ${escapeAttr(profile.name)}" data-voice-profile-name="${escapeAttr(profile.id)}" value="${escapeAttr(draft)}" maxlength="60" ${disabled() || busy ? "disabled" : ""}/>
    <small data-voice-profile-summary="${escapeAttr(profile.id)}">${escapeHtml(clips)} · ${escapeHtml(formatSeconds(profile.sample_seconds))} of remembered speech · updated ${escapeHtml(updated)}</small>
    <button type="button" class="secondary" data-voice-profile-toggle="${escapeAttr(profile.id)}" aria-expanded="${expanded}" aria-label="${expanded ? "Hide" : "Show"} clips for ${escapeAttr(profile.name)}">${expanded ? "Hide clips" : "Show clips"}</button>
    <button type="button" class="secondary" data-voice-profile-rename="${escapeAttr(profile.id)}" ${disabled() || busy ? "disabled" : ""}>Rename</button>
    <button type="button" class="secondary" data-voice-profile-delete="${escapeAttr(profile.id)}" ${disabled() || busy ? "disabled" : ""}>Delete</button>
    ${expanded ? profileDetailsHtml(profile, busy) : ""}
  </li>`;
}

async function loadSamples(profileId: string) {
  try {
    store.voiceProfileSamples.set(profileId, await invoke<VoiceProfileSample[]>("list_voice_profile_samples", { profileId }));
    store.voiceProfileSampleErrors.delete(profileId);
  } catch (error) {
    store.voiceProfileSampleErrors.set(profileId, `Could not load clips: ${String(error)}`);
  } finally {
    updateVoiceProfilesUI();
  }
}

async function togglePlaySample(sampleId: string) {
  if (playingClipKey() === sampleId) {
    stopClip();
    return;
  }
  if (sampleLoading.has(sampleId)) return;
  stopClip();
  sampleClipErrors.delete(sampleId);
  sampleLoading.add(sampleId);
  updateVoiceProfilesUI();
  try {
    await playClip(sampleId, await invoke<string>("get_voice_sample_audio", { sampleId }));
  } catch (error) {
    sampleClipErrors.set(sampleId, `Could not play this clip: ${String(error)}`);
  } finally {
    sampleLoading.delete(sampleId);
    updateVoiceProfilesUI();
  }
}

/// The "Voice profiles" section's markup, rendered as its own function so it
/// can be spliced into the Settings form (the "Recognize known voices"
/// toggle above it is a normal form field, saved with the rest of Settings)
/// while the list below it acts immediately, like AppFlowy's destinations.
export function voiceProfilesSectionHtml(): string {
  const status = store.voiceProfilesError
    ?? (!store.voiceProfilesLoaded ? "Loading voice profiles…"
      : store.voiceProfiles.length === 0 ? "No voices remembered yet."
        : "");
  return `<label class="toggle"><input type="checkbox" name="recognize_known_voices" ${store.settings.recognize_known_voices !== false ? "checked" : ""} ${disabled()}/><span><b>Recognize known voices in new meetings</b><small>After speaker identification finishes, matches each detected voice against voices you have named and remembered, and labels a confident match automatically. Never overwrites a name you typed yourself.</small></span></label>
    <b id="voice-profiles-heading">Voice profiles</b>
    <small>A voice profile is a fingerprint computed from audio you named and chose to remember; it stays only on this device.</small>
    <p id="voice-profiles-status" role="status">${escapeHtml(status)}</p>
    <ul id="voice-profiles-list">${store.voiceProfiles.map(voiceProfileRowHtml).join("")}</ul>
    <div><button type="button" class="secondary" id="forget-all-voices" ${disabled() || !store.voiceProfiles.length || store.voiceProfilesForgetBusy ? "disabled" : ""}>Forget all voices</button></div>`;
}

export function wireVoiceProfilesSection() {
  document.querySelectorAll<HTMLInputElement>("[data-voice-profile-name]").forEach((input) => {
    input.oninput = () => store.voiceProfileRenameDrafts.set(input.dataset.voiceProfileName!, input.value);
  });
  document.querySelectorAll<HTMLButtonElement>("[data-voice-profile-rename]").forEach((button) => {
    button.onclick = async () => {
      const id = button.dataset.voiceProfileRename!;
      const name = (store.voiceProfileRenameDrafts.get(id) ?? "").trim();
      if (!name) return;
      store.voiceProfilesBusy.add(id);
      updateVoiceProfilesUI();
      try {
        await invoke("rename_voice_profile", { id, name });
        await refreshVoiceProfiles();
      } catch (error) {
        notice(`Could not rename voice profile: ${String(error)}`, "error");
      } finally {
        store.voiceProfilesBusy.delete(id);
        updateVoiceProfilesUI();
      }
    };
  });
  document.querySelectorAll<HTMLButtonElement>("[data-voice-profile-delete]").forEach((button) => {
    button.onclick = async () => {
      const id = button.dataset.voiceProfileDelete!;
      const profile = store.voiceProfiles.find((p) => p.id === id);
      if (!window.confirm(`Forget the voice profile for "${profile?.name ?? "this person"}"? Meetings already named with it keep their name; only the remembered voice fingerprint is removed.`)) return;
      store.voiceProfilesBusy.add(id);
      updateVoiceProfilesUI();
      try {
        await invoke("delete_voice_profile", { id });
        store.voiceProfileRenameDrafts.delete(id);
        store.voiceProfileExpanded.delete(id);
        store.voiceProfileSamples.delete(id);
        await refreshVoiceProfiles();
      } catch (error) {
        notice(`Could not delete voice profile: ${String(error)}`, "error");
      } finally {
        store.voiceProfilesBusy.delete(id);
        updateVoiceProfilesUI();
      }
    };
  });
  document.querySelectorAll<HTMLButtonElement>("[data-voice-profile-toggle]").forEach((button) => {
    button.onclick = () => {
      const id = button.dataset.voiceProfileToggle!;
      if (store.voiceProfileExpanded.has(id)) {
        store.voiceProfileExpanded.delete(id);
        updateVoiceProfilesUI();
      } else {
        store.voiceProfileExpanded.add(id);
        updateVoiceProfilesUI();
        void loadSamples(id);
      }
    };
  });
  document.querySelectorAll<HTMLButtonElement>("[data-voice-sample-play]").forEach((button) => {
    button.onclick = () => void togglePlaySample(button.dataset.voiceSamplePlay!);
  });
  document.querySelectorAll<HTMLButtonElement>("[data-voice-sample-delete]").forEach((button) => {
    button.onclick = async () => {
      const sampleId = button.dataset.voiceSampleDelete!;
      const owner = Array.from(store.voiceProfileSamples.entries()).find(([, list]) => list.some((s) => s.id === sampleId))?.[0];
      if (!owner) return;
      if (!window.confirm("Delete this clip? The voice profile is recomputed from the remaining clips.")) return;
      if (playingClipKey() === sampleId) stopClip();
      store.voiceProfilesBusy.add(owner);
      updateVoiceProfilesUI();
      try {
        await invoke("delete_voice_profile_sample", { sampleId });
        await refreshVoiceProfiles();
        if (store.voiceProfiles.some((p) => p.id === owner)) await loadSamples(owner);
        else store.voiceProfileSamples.delete(owner);
      } catch (error) {
        notice(`Could not delete clip: ${String(error)}`, "error");
      } finally {
        store.voiceProfilesBusy.delete(owner);
        updateVoiceProfilesUI();
      }
    };
  });
  document.querySelectorAll<HTMLSelectElement>("[data-voice-profile-merge-target]").forEach((select) => {
    select.onchange = () => {
      store.voiceProfileMergeDrafts.set(select.dataset.voiceProfileMergeTarget!, select.value);
      const button = select.closest(".voice-merge")?.querySelector<HTMLButtonElement>("[data-voice-profile-merge]");
      if (button) button.disabled = !select.value;
    };
  });
  document.querySelectorAll<HTMLButtonElement>("[data-voice-profile-merge]").forEach((button) => {
    button.onclick = async () => {
      const sourceId = button.dataset.voiceProfileMerge!;
      const targetId = store.voiceProfileMergeDrafts.get(sourceId);
      const source = store.voiceProfiles.find((p) => p.id === sourceId);
      const target = store.voiceProfiles.find((p) => p.id === targetId);
      if (!source || !target || !targetId) return;
      if (!window.confirm(`Merge "${source.name}" into "${target.name}"? All of ${source.name}'s clips move to ${target.name}, and ${source.name} is removed. This cannot be undone.`)) return;
      store.voiceProfilesBusy.add(sourceId);
      store.voiceProfileMergeErrors.delete(sourceId);
      updateVoiceProfilesUI();
      try {
        await invoke("merge_voice_profiles", { sourceId, targetId });
        for (const map of [store.voiceProfileMergeDrafts, store.voiceProfileRenameDrafts, store.voiceProfileSamples]) map.delete(sourceId);
        store.voiceProfileExpanded.delete(sourceId);
        store.voiceProfileSamples.delete(targetId);
        await refreshVoiceProfiles();
        if (store.voiceProfileExpanded.has(targetId)) await loadSamples(targetId);
      } catch (error) {
        store.voiceProfileMergeErrors.set(sourceId, String(error));
      } finally {
        store.voiceProfilesBusy.delete(sourceId);
        updateVoiceProfilesUI();
      }
    };
  });
  const forgetAll = document.querySelector<HTMLButtonElement>("#forget-all-voices");
  if (forgetAll) forgetAll.onclick = async () => {
    if (!window.confirm("Forget every remembered voice? Meetings already named keep their names; only the remembered voice fingerprints are removed.")) return;
    store.voiceProfilesForgetBusy = true;
    updateVoiceProfilesUI();
    try {
      await invoke("forget_all_voice_profiles");
      store.voiceProfileRenameDrafts.clear();
      store.voiceProfileExpanded.clear();
      store.voiceProfileSamples.clear();
      await refreshVoiceProfiles();
    } catch (error) {
      notice(`Could not forget voice profiles: ${String(error)}`, "error");
    } finally {
      store.voiceProfilesForgetBusy = false;
      updateVoiceProfilesUI();
    }
  };
}

let renderedKey = "";

/// Polling-safe re-render, mirroring `settings/appflowy.ts`'s
/// `updateAppFlowySettingsUI`: re-renders the list only when its underlying
/// data actually changed, and otherwise carries unsaved rename drafts, focus,
/// and caret position across the replace.
export function updateVoiceProfilesUI() {
  const section = document.querySelector<HTMLElement>("#voice-profiles-section");
  if (!section) return;
  const key = JSON.stringify([
    store.voiceProfiles,
    store.voiceProfilesError,
    store.voiceProfilesLoaded,
    store.voiceProfilesForgetBusy,
    Array.from(store.voiceProfilesBusy),
    store.settings.recognize_known_voices,
    disabled(),
    Array.from(store.voiceProfileExpanded),
    Array.from(store.voiceProfileSamples),
    Array.from(store.voiceProfileSampleErrors),
    Array.from(store.voiceProfileMergeErrors),
    playingClipKey(),
    Array.from(sampleLoading),
    Array.from(sampleClipErrors),
  ]);
  if (key === renderedKey && section.childElementCount) return;
  const active = document.activeElement as HTMLInputElement | null;
  const focusKey = active && section.contains(active) ? active.name || active.dataset.voiceProfileName || "" : "";
  const caret = focusKey && typeof active?.selectionStart === "number" ? [active.selectionStart, active.selectionEnd] : null;
  section.innerHTML = voiceProfilesSectionHtml();
  renderedKey = key;
  wireVoiceProfilesSection();
  if (focusKey) {
    const el = section.querySelector<HTMLInputElement>(`[name="${CSS.escape(focusKey)}"], [data-voice-profile-name="${CSS.escape(focusKey)}"]`);
    el?.focus();
    if (el && caret) try { el.setSelectionRange(caret[0], caret[1]); } catch { /* not a text input */ }
  }
}
