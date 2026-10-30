import { invoke, preview } from "../api";
import { disabled, store } from "../store";
import type { AtomikKbConnectionTest, AtomikKbPageInfo, AtomikKbState } from "../types";
import { escapeAttr, escapeHtml } from "../util/escape";
import { updateRecordingUI } from "../views/library";

export function atomikKbJob(id: string) {
  return store.atomikKbState.jobs.find((job) => job.recording_id === id);
}

/// Status chip text for a meeting row; `null` while the integration is off so
/// people who never enable it see nothing extra.
export function atomikKbSummary(recordingId: string): string | null {
  const state = store.atomikKbState;
  if (!state.enabled) return null;
  const job = atomikKbJob(recordingId);
  if (!job) return state.has_token ? (state.auto_export ? "Atomik KB export queued" : "Not exported to Atomik KB") : "Add Atomik KB token to export";
  if (job.status === "exporting") return "Exporting to Atomik KB…";
  if (job.status === "completed") return job.export_index > 1 ? `Exported to Atomik KB (${job.export_index})` : "Exported to Atomik KB";
  if (job.status === "queued") return "Atomik KB export queued";
  return "Atomik KB export failed";
}

export async function refreshAtomikKbState() {
  if (preview || !store.backendReady || document.hidden || store.atomikKbPoll) return;
  store.atomikKbPoll = true;
  try {
    store.atomikKbState = await invoke<AtomikKbState>("get_atomik_kb_state");
  } catch (error) {
    store.atomikKbState = { ...store.atomikKbState, error: `Could not check Atomik KB export: ${String(error)}` };
  } finally {
    store.atomikKbPoll = false;
    updateRecordingUI();
    updateAtomikKbSettingsUI();
  }
  // A cheap local-journal read; the section only re-renders if the list changed.
  await refreshAtomikKbPages();
}

let renderedKey = "";

/// Read-only local journal of every page this integration has created, for
/// the "Exported pages" list.
export async function refreshAtomikKbPages() {
  if (preview || !store.backendReady) return;
  try {
    store.atomikKbPages = await invoke<AtomikKbPageInfo[]>("atomik_kb_list_pages");
  } catch {
    store.atomikKbPages = [];
  }
  updateAtomikKbSettingsUI();
}

function tokenStatusText(): string {
  if (store.atomikKbMessage) return store.atomikKbMessage;
  return store.atomikKbState.has_token
    ? "A token is saved in the OS credential store (it is never shown again)."
    : "No token saved. In atomik-kb: Settings → MCP → add client, copy the token once, and paste it here.";
}

function testResultHtml(): string {
  const test: AtomikKbConnectionTest | null = store.atomikKbTest;
  if (!test) return "";
  return `<p id="atomik-kb-test-result" role="status" class="${test.ok ? "ok" : "bad"}">${escapeHtml(test.message)}</p>`;
}

/// Read-only list of every page this integration has created locally. This
/// app itself never moves, renames, or deletes anything listed here.
function exportedPagesHtml(): string {
  if (!store.atomikKbPages.length) return "";
  const rows = store.atomikKbPages.map((p) => `<li><span>${escapeHtml(p.name)}</span><small>${escapeHtml(p.kind)} · ${escapeHtml(p.status)}</small></li>`).join("");
  return `<details id="atomik-kb-exported-pages"><summary>Exported pages (${store.atomikKbPages.length})</summary><ul>${rows}</ul></details>`;
}

/// Rendered as its own function so `updateAtomikKbSettingsUI` can replace just
/// this section in place after a token/test action, rather than a full
/// `render()` -- which would also reset the visible page back to the dashboard.
export function atomikKbSettingsSectionHtml(): string {
  const busy = store.atomikKbBusy ? "disabled" : "";
  return `<b id="atomik-kb-heading">Atomik KB export</b>
    <label class="toggle"><input type="checkbox" name="atomik_kb_enabled" ${store.settings.atomik_kb_enabled ? "checked" : ""} ${disabled()}/><span><b>Export meetings to Atomik KB</b><small>Sends only the transcript and summary text (never audio) to your local atomik-kb app on this computer (127.0.0.1 only), under "My daily meetings" › a page per date › a page per meeting. atomik-kb must be open and unlocked. Off by default.</small></span></label>
    <label class="toggle"><input type="checkbox" name="atomik_kb_auto_export" ${store.settings.atomik_kb_auto_export ? "checked" : ""} ${disabled() || !store.settings.atomik_kb_enabled ? "disabled" : ""}/><span><b>Export to Atomik KB automatically</b><small>Off by default even once export is enabled -- otherwise use the "Export to Atomik KB" button on each meeting.</small></span></label>
    <label>Atomik KB port<input type="number" name="atomik_kb_port" min="1" max="65535" value="${escapeAttr(String(store.settings.atomik_kb_port ?? 47811))}" ${disabled()}/><small>Settings → MCP in atomik-kb (default 47811). The host is always 127.0.0.1.</small></label>
    <label>Space name<input type="text" name="atomik_kb_space_name" maxlength="200" value="${escapeAttr(store.settings.atomik_kb_space_name ?? "My Daily Meeting")}" ${disabled()}/><small>Matched by exact name (ignoring case). It must already exist and the client must have write access; this app never creates a space or uses another one.</small></label>
    <p id="atomik-kb-token-status">${escapeHtml(tokenStatusText())}</p>
    <label>Token<input type="password" id="atomik-kb-token" autocomplete="off" spellcheck="false" placeholder="kbm_…" ${disabled()}/><small>Stored only in the OS credential store -- never in settings, the database, or logs.</small></label>
    <div><button type="button" class="secondary" id="atomik-kb-save-token" ${disabled()} ${busy}>Save token</button> <button type="button" class="secondary" id="atomik-kb-clear-token" ${disabled()} ${busy || !store.atomikKbState.has_token ? "disabled" : ""}>Clear token</button> <button type="button" class="secondary" id="atomik-kb-test" ${disabled()} ${busy}>Test Atomik KB connection</button></div>
    ${testResultHtml()}
    ${exportedPagesHtml()}`;
}

export function wireAtomikKbSettingsSection() {
  const act = async (task: () => Promise<void>) => {
    store.atomikKbBusy = true; updateAtomikKbSettingsUI();
    try { await task(); }
    finally { store.atomikKbBusy = false; updateAtomikKbSettingsUI(); }
  };
  document.querySelector("#atomik-kb-save-token")?.addEventListener("click", () => act(async () => {
    const field = document.querySelector<HTMLInputElement>("#atomik-kb-token");
    const token = (field?.value ?? "").trim();
    if (!token) { store.atomikKbMessage = "Paste the token first."; return; }
    try {
      await invoke("atomik_kb_save_token", { token });
      if (field) field.value = "";
      store.atomikKbMessage = "Token saved in the OS credential store.";
      store.atomikKbTest = null;
      await refreshAtomikKbState();
    } catch (error) { store.atomikKbMessage = `Could not save token: ${String(error)}`; }
  }));
  document.querySelector("#atomik-kb-clear-token")?.addEventListener("click", () => act(async () => {
    try {
      await invoke("atomik_kb_clear_token");
      store.atomikKbMessage = "Token removed from the OS credential store.";
      store.atomikKbTest = null;
      await refreshAtomikKbState();
    } catch (error) { store.atomikKbMessage = `Could not clear token: ${String(error)}`; }
  }));
  document.querySelector("#atomik-kb-test")?.addEventListener("click", () => act(async () => {
    const port = Number(document.querySelector<HTMLInputElement>('[name="atomik_kb_port"]')?.value) || undefined;
    const spaceName = (document.querySelector<HTMLInputElement>('[name="atomik_kb_space_name"]')?.value ?? "").trim() || undefined;
    store.atomikKbMessage = null;
    try { store.atomikKbTest = await invoke<AtomikKbConnectionTest>("atomik_kb_test_connection", { port, spaceName }); }
    catch (error) { store.atomikKbTest = { ok: false, message: `Test failed: ${String(error)}`, space_found: false, writable: false }; }
  }));
  // Auto-export depends on the live (possibly unsaved) enable checkbox, not the saved setting.
  const enabled = document.querySelector<HTMLInputElement>('[name="atomik_kb_enabled"]');
  enabled?.addEventListener("change", () => {
    const auto = document.querySelector<HTMLInputElement>('[name="atomik_kb_auto_export"]');
    if (auto) auto.disabled = Boolean(disabled()) || !enabled.checked;
  });
}

/// Polling calls this every couple of seconds, so it must never discard what
/// the user is editing (including a half-pasted token): it re-renders only
/// when the visible structure changes, then carries unsaved control values,
/// focus and caret across.
export function updateAtomikKbSettingsUI() {
  const section = document.querySelector<HTMLElement>("#atomik-kb-settings-section");
  if (!section) return;
  const key = JSON.stringify([store.atomikKbState.has_token, store.atomikKbBusy, store.atomikKbMessage, store.atomikKbTest, store.atomikKbPages, disabled()]);
  if (key === renderedKey && section.childElementCount) return;
  const controls = Array.from(section.querySelectorAll<HTMLInputElement>("input"));
  const saved = new Map(controls.map((c) => [c.name || c.id, c.type === "checkbox" ? c.checked : c.value]));
  const hadRender = section.childElementCount > 0 && renderedKey !== "";
  const active = document.activeElement as HTMLInputElement | null;
  const focusKey = active && section.contains(active) ? active.name || active.id : "";
  const caret = focusKey && typeof active!.selectionStart === "number" ? [active!.selectionStart, active!.selectionEnd] : null;
  section.innerHTML = atomikKbSettingsSectionHtml();
  renderedKey = key;
  if (hadRender) {
    for (const c of Array.from(section.querySelectorAll<HTMLInputElement>("input"))) {
      const value = saved.get(c.name || c.id);
      if (value === undefined) continue;
      if (c.type === "checkbox") c.checked = value as boolean;
      else c.value = value as string;
    }
    const enabled = section.querySelector<HTMLInputElement>('[name="atomik_kb_enabled"]');
    const auto = section.querySelector<HTMLInputElement>('[name="atomik_kb_auto_export"]');
    if (enabled && auto) auto.disabled = Boolean(disabled()) || !enabled.checked;
  }
  wireAtomikKbSettingsSection();
  if (focusKey) {
    const el = section.querySelector<HTMLInputElement>(`[name="${CSS.escape(focusKey)}"], #${CSS.escape(focusKey)}`);
    el?.focus();
    if (el && caret) try { el.setSelectionRange(caret[0], caret[1]); } catch { /* not a text input */ }
  }
}
