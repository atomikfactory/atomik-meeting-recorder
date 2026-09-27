import { invoke, preview } from "../api";
import { notice } from "../ui/notice";
import { disabled, store } from "../store";
import type { VoiceProfileInfo } from "../types";
import { escapeAttr, escapeHtml } from "../util/escape";

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

function voiceProfileRowHtml(profile: VoiceProfileInfo): string {
  const busy = store.voiceProfilesBusy.has(profile.id);
  const draft = store.voiceProfileRenameDrafts.get(profile.id) ?? profile.name;
  const updated = new Date(profile.updated_at * 1000).toLocaleString();
  return `<li data-voice-profile="${escapeAttr(profile.id)}">
    <input type="text" data-voice-profile-name="${escapeAttr(profile.id)}" value="${escapeAttr(draft)}" maxlength="60" ${disabled() || busy ? "disabled" : ""}/>
    <small>${escapeHtml(formatSeconds(profile.sample_seconds))} of remembered speech · updated ${escapeHtml(updated)}</small>
    <button type="button" class="secondary" data-voice-profile-rename="${escapeAttr(profile.id)}" ${disabled() || busy ? "disabled" : ""}>Rename</button>
    <button type="button" class="secondary" data-voice-profile-delete="${escapeAttr(profile.id)}" ${disabled() || busy ? "disabled" : ""}>Delete</button>
  </li>`;
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
        await refreshVoiceProfiles();
      } catch (error) {
        notice(`Could not delete voice profile: ${String(error)}`, "error");
      } finally {
        store.voiceProfilesBusy.delete(id);
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
