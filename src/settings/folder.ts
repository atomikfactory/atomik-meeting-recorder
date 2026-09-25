import { invoke, preview } from "../api";
import { notice } from "../ui/notice";
import { store } from "../store";
import type { AppState, MoveSummary, Settings } from "../types";
import { setTextIfChanged } from "../util/escape";
import { refreshRecordingState } from "../views/library";

export async function refreshAppState() {
  try {
    store.appState = await invoke<AppState>("get_app_state");
  } catch (error) {
    notice(String(error), "error");
  }
}

export function refreshRecordingsFolderSection() {
  setTextIfChanged(document.querySelector("#recordings-folder-path"), store.appState.recordings_folder);
  const reset = document.querySelector<HTMLButtonElement>("#reset-recordings-folder");
  if (reset) {
    const shouldDisable = preview || !store.backendReady || !store.settings.recordings_folder;
    reset.disabled = shouldDisable;
    if (shouldDisable) reset.setAttribute("aria-disabled", "true");
    else reset.removeAttribute("aria-disabled");
  }
}

export function wireFolderSection() {
  document.querySelector("#choose-recordings-folder")?.addEventListener("click", async () => {
    const status = document.querySelector<HTMLElement>("#recordings-folder-status");
    try {
      const saved = await invoke<Settings | null>("choose_recordings_folder");
      if (!saved) return; // user cancelled the folder picker
      store.settings = { ...store.settings, ...saved };
      if (status) status.textContent = "Recordings folder updated. New recordings will use it immediately.";
      await refreshAppState();
      refreshRecordingsFolderSection();
    } catch (error) {
      if (status) status.textContent = String(error);
      notice(String(error), "error");
    }
  });
  document.querySelector("#reset-recordings-folder")?.addEventListener("click", async () => {
    const status = document.querySelector<HTMLElement>("#recordings-folder-status");
    try {
      const saved = await invoke<Settings>("reset_recordings_folder");
      store.settings = { ...store.settings, ...saved };
      if (status) status.textContent = "Recordings folder reset to the default location.";
      await refreshAppState();
      refreshRecordingsFolderSection();
    } catch (error) {
      if (status) status.textContent = String(error);
      notice(String(error), "error");
    }
  });
  document.querySelector("#open-recordings-folder-btn")?.addEventListener("click", () =>
    invoke("open_recordings_folder").catch((error) => notice(String(error), "error")),
  );
  document.querySelector("#move-recordings")?.addEventListener("click", async (event) => {
    const button = event.currentTarget as HTMLButtonElement;
    const status = document.querySelector<HTMLElement>("#recordings-folder-status");
    button.disabled = true;
    if (status) status.textContent = "Moving recordings…";
    try {
      const summary = await invoke<MoveSummary>("move_recordings_to_configured_folder");
      const failed = summary.results.filter((r) => !r.moved);
      const moved = summary.results.filter((r) => r.moved);
      if (status) {
        status.textContent = summary.results.length === 0
          ? "Nothing to move; every recording is already in this folder."
          : `Moved ${moved.length} recording(s).${failed.length ? ` ${failed.length} failed: ${failed.map((r) => r.error).join("; ")}` : ""}`;
      }
      await refreshRecordingState(true);
    } catch (error) {
      if (status) status.textContent = String(error);
      notice(String(error), "error");
    } finally {
      button.disabled = preview || !store.backendReady;
    }
  });
}
