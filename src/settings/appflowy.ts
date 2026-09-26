import { invoke, preview } from "../api";
import { notice } from "../ui/notice";
import { disabled, store } from "../store";
import type { AppFlowyDestination, AppFlowyPageInfo, AppFlowyState, AppFlowyWorkspace } from "../types";
import { escapeAttr, escapeHtml } from "../util/escape";
import { updateRecordingUI } from "../views/library";

/// Opens an AppFlowy Web URL the backend itself constructed (never a URL
/// this frontend builds) in the system browser, via the same
/// domain-restricted `open_appflowy_url` command "Open in AppFlowy" uses.
export async function openAppFlowyUrl(url: string) {
  try { await invoke("open_appflowy_url", { url }); }
  catch (error) { notice(`Could not open AppFlowy: ${String(error)}`, "error"); }
}

export function appflowyJob(id: string) {
  return store.appflowyState.jobs.find((job) => job.recording_id === id);
}

export function appflowySummary(recordingId: string) {
  if (!store.appflowyState.enabled) return "AppFlowy export off";
  const job = appflowyJob(recordingId);
  if (!job) return store.appflowyState.connected ? (store.appflowyState.auto_export ? "Export queued" : "Not exported") : "Sign in to export";
  if (job.status === "exporting") return "Exporting to AppFlowy…";
  if (job.status === "completed") return job.export_index > 1 ? `Exported to AppFlowy (${job.export_index})` : "Exported to AppFlowy";
  if (job.status === "queued") return "Export queued";
  return "AppFlowy export failed";
}

export async function refreshAppFlowyState() {
  if (preview || !store.backendReady || document.hidden || store.appflowyPoll) return;
  store.appflowyPoll = true;
  try {
    store.appflowyState = await invoke<AppFlowyState>("get_appflowy_state");
  } catch (error) {
    store.appflowyState = { ...store.appflowyState, error: `Could not check AppFlowy export: ${String(error)}` };
  } finally {
    store.appflowyPoll = false;
    updateRecordingUI();
    updateAppFlowySettingsUI();
  }
  // A session restored at startup never passes through verify, so load the
  // workspace list the first time the backend reports a connection.
  if (store.appflowyState.connected && !workspacesRequested) {
    workspacesRequested = true;
    await refreshAppFlowyWorkspaces();
    await refreshAppFlowyDestinations();
    await refreshAppFlowyPages();
  } else if (!store.appflowyState.connected) {
    workspacesRequested = false;
  }
}

let workspacesRequested = false;
let renderedKey = "";

export async function refreshAppFlowyWorkspaces() {
  if (preview || !store.backendReady || !store.appflowyState.connected) return;
  try {
    store.appflowyWorkspaces = await invoke<AppFlowyWorkspace[]>("appflowy_list_workspaces");
  } catch {
    store.appflowyWorkspaces = [];
  }
  updateAppFlowySettingsUI();
}

/// Read-only list of the workspace's Spaces (and their top-level pages) for
/// the "Destination" picker -- requires a workspace to already be selected
/// and saved, so it's a no-op until then (the picker just shows "Automatic").
export async function refreshAppFlowyDestinations() {
  if (preview || !store.backendReady || !store.appflowyState.connected) return;
  try {
    store.appflowyDestinations = await invoke<AppFlowyDestination[]>("appflowy_list_destinations");
  } catch {
    store.appflowyDestinations = [];
  }
  updateAppFlowySettingsUI();
}

/// Read-only local journal of every page this integration has ever
/// planned/created, for the "Previously created pages" list -- lets a user
/// find (and manually move) a page left behind by an older, buggy export
/// (e.g. one created directly under the workspace root before this fix).
export async function refreshAppFlowyPages() {
  if (preview || !store.backendReady) return;
  try {
    store.appflowyPages = await invoke<AppFlowyPageInfo[]>("appflowy_list_pages");
  } catch {
    store.appflowyPages = [];
  }
  updateAppFlowySettingsUI();
}

/// "Workspace › Space (or page) › My daily meetings" preview text for
/// `parentId` (defaults to the saved setting), from already-loaded state
/// only (no extra round trip on every keystroke/render).
export function destinationPathText(parentId = store.settings.appflowy_parent_view_id ?? ""): string {
  const workspace = store.appflowyWorkspaces.find((w) => w.workspace_id === (store.settings.appflowy_workspace_id ?? ""));
  const workspaceName = workspace?.workspace_name ?? workspace?.workspace_id ?? "Workspace";
  if (!parentId) return `${workspaceName} › Automatic (General space) › My daily meetings`;
  const destination = store.appflowyDestinations.find((d) => d.view_id === parentId);
  return `${workspaceName} › ${destination ? destination.name : parentId} › My daily meetings`;
}

/// The AppFlowy Settings section's markup depends on three mutually exclusive
/// states (signed out, code sent, signed in); rendered as its own function so
/// `updateAppFlowySettingsUI` can replace just this section in place after an
/// auth action, rather than a full `render()` -- which would also reset the
/// visible page back to the dashboard (page switching here is plain DOM
/// show/hide, not state-driven).
export function appflowySettingsSectionHtml(): string {
  return `<b id="appflowy-heading">AppFlowy export</b>
    <label class="toggle"><input type="checkbox" name="appflowy_enabled" ${store.settings.appflowy_enabled ? "checked" : ""} ${disabled()}/><span><b>Export meetings to AppFlowy</b><small>Sends only the transcript and summary text (never audio) to your chosen AppFlowy Cloud workspace, under "My daily meetings" › a page per date › a page per meeting. Off by default.</small></span></label>
    <label class="toggle"><input type="checkbox" name="appflowy_auto_export" ${store.settings.appflowy_auto_export ? "checked" : ""} ${disabled() || !store.settings.appflowy_enabled ? "disabled" : ""}/><span><b>Export automatically</b><small>Off by default even once export is enabled -- otherwise use the "Export to AppFlowy" button on each meeting.</small></span></label>
    <label>AppFlowy server<input type="text" name="appflowy_base_url" value="${escapeAttr(store.settings.appflowy_base_url ?? "https://beta.appflowy.cloud")}" ${disabled()}/><small>https:// required (self-hosted supported); the official cloud is the default.</small></label>
    <label class="toggle"><input type="checkbox" name="appflowy_use_native_block" ${store.settings.appflowy_use_native_block !== false ? "checked" : ""} ${disabled()}/><span><b>Use the native AI Meeting Notes block</b><small>Unchecking sends a plain heading + toggle-list fallback instead, for a client that renders the native block blank.</small></span></label>
    <p id="appflowy-auth-status">${escapeHtml(store.appflowyAuthError ?? (store.appflowyState.connected ? `Signed in as ${store.appflowyState.email ?? "unknown"}.` : store.appflowyOtpSent ? `Code sent to ${store.appflowyOtpEmail}. Enter it below.` : "Not signed in."))}</p>
    ${store.appflowyState.connected ? `<div><button type="button" class="secondary" id="appflowy-sign-out" ${disabled()}>Sign out</button></div>
    <label>Workspace<select name="appflowy_workspace_id" ${disabled()}>${store.appflowyWorkspaces.map((w) => `<option value="${escapeAttr(w.workspace_id)}" ${w.workspace_id === (store.settings.appflowy_workspace_id ?? "") ? "selected" : ""}>${escapeHtml(w.workspace_name ?? w.workspace_id)}</option>`).join("") || `<option value="">No workspaces found</option>`}</select></label>
    <label>Destination<select name="appflowy_parent_view_id" ${disabled()}><option value="">Automatic (General space)</option>${store.appflowyDestinations.map((d) => `<option value="${escapeAttr(d.view_id)}" ${d.view_id === (store.settings.appflowy_parent_view_id ?? "") ? "selected" : ""}>${d.depth > 0 ? "&nbsp;&nbsp;" : ""}${escapeHtml(d.name)}</option>`).join("")}</select><small id="appflowy-destination-path">${escapeHtml(destinationPathText())}</small></label>` : `
    <label>Email<input type="email" id="appflowy-email" value="${escapeAttr(store.appflowyOtpEmail)}" ${disabled()}/></label>
    <div><button type="button" class="secondary" id="appflowy-send-code" ${disabled() || store.appflowyAuthBusy ? "disabled" : ""}>Send sign-in code</button></div>
    ${store.appflowyOtpSent ? `<label>Code from email<input type="text" id="appflowy-code" inputmode="numeric" ${disabled()}/></label>
    <div><button type="button" class="secondary" id="appflowy-verify-code" ${disabled() || store.appflowyAuthBusy ? "disabled" : ""}>Verify code</button></div>` : ""}`}
    ${appflowyPreviouslyCreatedPagesHtml()}`;
}

/// Read-only list of every page this integration has ever planned/created
/// locally, so a user can find (and, if they want, manually move in
/// AppFlowy) a page that was left orphaned by an older, buggy export --
/// including one created directly under the workspace root before this fix.
/// This app itself never moves, renames, or deletes anything listed here.
function appflowyPreviouslyCreatedPagesHtml(): string {
  if (!store.appflowyPages.length) return "";
  const rows = store.appflowyPages.map((p) => `<li><span>${escapeHtml(p.name)}</span><small>${escapeHtml(p.kind)} · ${escapeHtml(p.status)}</small>${p.url ? `<button type="button" class="secondary" data-open-appflowy-page="${escapeAttr(p.url)}">Open</button>` : ""}</li>`).join("");
  return `<details id="appflowy-previous-pages"><summary>Previously created pages (${store.appflowyPages.length})</summary><ul>${rows}</ul></details>`;
}

export function wireAppFlowySettingsSection() {
  document.querySelector("#appflowy-send-code")?.addEventListener("click", async () => {
    const email = (document.querySelector<HTMLInputElement>("#appflowy-email")?.value ?? "").trim();
    if (!email) return;
    store.appflowyAuthBusy = true; store.appflowyAuthError = null; updateAppFlowySettingsUI();
    try { await invoke("appflowy_request_otp", { email }); store.appflowyOtpEmail = email; store.appflowyOtpSent = true; }
    catch (error) { store.appflowyAuthError = `Could not send sign-in code: ${String(error)}`; }
    finally { store.appflowyAuthBusy = false; updateAppFlowySettingsUI(); }
  });
  document.querySelector("#appflowy-verify-code")?.addEventListener("click", async () => {
    const code = (document.querySelector<HTMLInputElement>("#appflowy-code")?.value ?? "").trim();
    if (!code) return;
    store.appflowyAuthBusy = true; store.appflowyAuthError = null; updateAppFlowySettingsUI();
    try { await invoke("appflowy_verify_otp", { email: store.appflowyOtpEmail, code }); store.appflowyOtpSent = false; await refreshAppFlowyState(); await refreshAppFlowyWorkspaces(); }
    catch (error) { store.appflowyAuthError = `Could not verify code: ${String(error)}`; }
    finally { store.appflowyAuthBusy = false; updateAppFlowySettingsUI(); }
  });
  // Auto-export depends on the live (possibly unsaved) enable checkbox, not the saved setting.
  const enabled = document.querySelector<HTMLInputElement>('[name="appflowy_enabled"]');
  enabled?.addEventListener("change", () => {
    const auto = document.querySelector<HTMLInputElement>('[name="appflowy_auto_export"]');
    if (auto) auto.disabled = Boolean(disabled()) || !enabled.checked;
  });
  document.querySelector("#appflowy-sign-out")?.addEventListener("click", async () => {
    try { await invoke("appflowy_sign_out"); store.appflowyOtpSent = false; store.appflowyOtpEmail = ""; await refreshAppFlowyState(); }
    catch (error) { notice(`Could not sign out of AppFlowy: ${String(error)}`, "error"); }
    finally { updateAppFlowySettingsUI(); }
  });
  // Live-updates the "Workspace › Space › My daily meetings" preview as the
  // user picks a destination, without waiting for Save (which is what
  // actually persists it).
  document.querySelector<HTMLSelectElement>('[name="appflowy_parent_view_id"]')?.addEventListener("change", (event) => {
    const path = document.querySelector("#appflowy-destination-path");
    if (path) path.textContent = destinationPathText((event.target as HTMLSelectElement).value);
  });
  document.querySelectorAll<HTMLButtonElement>("[data-open-appflowy-page]").forEach((button) =>
    (button.onclick = () => openAppFlowyUrl(button.dataset.openAppflowyPage!)));
}

/// Polling calls this every couple of seconds, so it must never discard what
/// the user is editing: it re-renders only when the auth/workspace structure
/// changes, and then carries unsaved control values, focus and caret across.
export function updateAppFlowySettingsUI() {
  const section = document.querySelector<HTMLElement>("#appflowy-settings-section");
  if (!section) return;
  const s = store.appflowyState;
  const key = JSON.stringify([s.connected, s.email, store.appflowyOtpSent, store.appflowyOtpEmail, store.appflowyAuthBusy, store.appflowyAuthError, store.appflowyWorkspaces, store.appflowyDestinations, store.appflowyPages, store.settings.appflowy_workspace_id, disabled()]);
  if (key === renderedKey && section.childElementCount) return;
  const controls = Array.from(section.querySelectorAll<HTMLInputElement | HTMLSelectElement>("input, select"));
  const saved = new Map(controls.map((c) => [c.name || c.id, c instanceof HTMLInputElement && c.type === "checkbox" ? c.checked : c.value]));
  const hadRender = section.childElementCount > 0 && renderedKey !== "";
  const active = document.activeElement as HTMLInputElement | null;
  const focusKey = active && section.contains(active) ? active.name || active.id : "";
  const caret = focusKey && typeof active!.selectionStart === "number" ? [active!.selectionStart, active!.selectionEnd] : null;
  section.innerHTML = appflowySettingsSectionHtml();
  renderedKey = key;
  if (hadRender) {
    for (const c of Array.from(section.querySelectorAll<HTMLInputElement | HTMLSelectElement>("input, select"))) {
      const value = saved.get(c.name || c.id);
      if (value === undefined) continue;
      if (c instanceof HTMLInputElement && c.type === "checkbox") c.checked = value as boolean;
      else if (c instanceof HTMLSelectElement) { if (Array.from(c.options).some((o) => o.value === value)) c.value = value as string; }
      else c.value = value as string;
    }
    const enabled = section.querySelector<HTMLInputElement>('[name="appflowy_enabled"]');
    const auto = section.querySelector<HTMLInputElement>('[name="appflowy_auto_export"]');
    if (enabled && auto) auto.disabled = Boolean(disabled()) || !enabled.checked;
  }
  wireAppFlowySettingsSection();
  if (focusKey) {
    const el = section.querySelector<HTMLInputElement>(`[name="${CSS.escape(focusKey)}"], #${CSS.escape(focusKey)}`);
    el?.focus();
    if (el && caret) try { el.setSelectionRange(caret[0], caret[1]); } catch { /* not a text input */ }
  }
}
