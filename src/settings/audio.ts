import { invoke, preview } from "../api";
import { notice } from "../ui/notice";
import { disabled, recordingBusy, store } from "../store";
import type { AudioDevice, AudioInventory, Settings } from "../types";
import { escapeAttr, escapeHtml, setTextIfChanged } from "../util/escape";
import { sourceInputId, sourceSummary } from "../util/format";

export function rememberSource(device: Pick<AudioDevice, "id" | "name" | "direction">) {
  if (device.id) {
    store.draftSources.set(device.id, {
      id: device.id,
      name: device.name,
      direction: device.direction,
    });
  }
}

export function deviceRows() {
  const inventory = store.inventory;
  const known = new Map((inventory?.devices ?? []).filter((device) => device.id).map((device) => [device.id!, device]));
  const unavailable = [...store.selectedSourceIds]
    .filter((id) => !known.has(id))
    .map((id) => store.draftSources.get(id) ?? { id, name: "Previously selected audio device", direction: "input" as const });
  const rows = [...(inventory?.devices ?? [])];
  for (const source of unavailable) rows.push({ id: source.id, name: source.name, direction: source.direction, is_default: false, default_format: null, selectable: true, unavailable_reason: "Currently unavailable. Kept selected until you remove it." });
  if (!rows.length) return `<p class="device-empty">No audio endpoints were reported. Your saved choices will remain unchanged.</p>`;
  return rows.map((device) => {
    const id = device.id;
    const checked = id && store.selectedSourceIds.has(id) ? "checked" : "";
    const canChoose = Boolean(id && device.selectable) || Boolean(id && checked);
    const note = device.unavailable_reason ?? (device.default_format ? `Default format: ${device.default_format}` : "Default format unavailable");
    const inputId = sourceInputId(id ?? "");
    return `<label class="device-row ${!canChoose ? "device-disabled" : ""}" for="${escapeAttr(inputId)}"><input id="${escapeAttr(inputId)}" type="checkbox" class="audio-source" value="${escapeAttr(id ?? "")}" ${checked} ${canChoose && !preview && store.backendReady && !store.audioSaveInFlight && !recordingBusy() ? "" : "disabled"}/><span><b>${escapeHtml(device.name)}</b><small>${device.direction === "input" ? "Microphone / input" : "System output"}${device.is_default ? " (system default)" : ""}</small><small>${escapeHtml(note)}</small></span></label>`;
  }).join("");
}

export function refreshDeviceSection() {
  const list = document.querySelector<HTMLElement>("#device-list");
  const focusedId = document.activeElement instanceof HTMLElement ? document.activeElement.id : "";
  const nextRows = deviceRows();
  if (list && store.renderedDeviceRows !== nextRows) {
    list.innerHTML = nextRows;
    store.renderedDeviceRows = nextRows;
    if (focusedId) document.getElementById(focusedId)?.focus();
  }
  const errors = document.querySelector<HTMLElement>("#device-errors");
  setTextIfChanged(errors, store.inventory?.errors.join(" ") ?? "");
  const backend = document.querySelector<HTMLElement>("#inventory-backend");
  setTextIfChanged(
    backend,
    store.inventory?.backend ? `Backend: ${store.inventory.backend}` : "Backend unavailable",
  );
  document.querySelectorAll<HTMLInputElement>(".audio-source").forEach((input) => input.onchange = () => {
    if (input.checked) {
      store.selectedSourceIds.add(input.value);
      const device = store.inventory?.devices.find((item) => item.id === input.value);
      if (device) rememberSource(device);
    } else store.selectedSourceIds.delete(input.value);
  });
}

export async function refreshDevices() {
  if (preview || !store.backendReady || store.inventoryRefreshing || document.hidden) return;
  store.inventoryRefreshing = true;
  const refresh = document.querySelector<HTMLButtonElement>("#refresh-devices");
  if (refresh) refresh.disabled = true;
  try {
    store.inventory = await invoke<AudioInventory>("get_audio_devices");
    for (const device of store.inventory.devices) {
      if (device.id && store.selectedSourceIds.has(device.id)) rememberSource(device);
    }
    refreshDeviceSection();
  }
  catch (error) { const errors = document.querySelector<HTMLElement>("#device-errors"); if (errors) errors.textContent = `Could not refresh audio devices: ${String(error)}`; }
  finally { store.inventoryRefreshing = false; if (refresh) refresh.disabled = false; }
}

export function wireAudioSourcesSection() {
  document.querySelector("#refresh-devices")?.addEventListener("click", refreshDevices);
  document.querySelector("#save-audio-sources")?.addEventListener("click", async () => {
    if (store.audioSaveInFlight) return;
    store.audioSaveInFlight = true;
    refreshDeviceSection();
    const button = document.querySelector<HTMLButtonElement>("#save-audio-sources");
    if (button) button.disabled = true;
    try {
      const saved = await invoke<Settings>("save_audio_selection", { sourceIds: [...store.selectedSourceIds] });
      store.settings = {
        ...saved,
        audio_sources: saved.audio_sources.map((source) => store.draftSources.get(source.id) ?? source),
      };
      store.selectedSourceIds = new Set(store.settings.audio_sources.map((source) => source.id));
      store.draftSources = new Map(store.settings.audio_sources.map((source) => [source.id, source]));
      notice("Audio sources saved locally for your next recording.");
      const summary = document.querySelector<HTMLElement>("#audio-summary");
      if (summary) summary.textContent = sourceSummary(store.settings.audio_sources);
    } catch (error) {
      notice(`Could not save audio sources: ${String(error)}`, "error");
    } finally {
      store.audioSaveInFlight = false;
      if (button) button.disabled = preview || !store.backendReady;
      refreshDeviceSection();
    }
  });
}
