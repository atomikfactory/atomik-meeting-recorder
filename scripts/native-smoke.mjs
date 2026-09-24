// Optional Windows verification against a locally launched WebView2 debug build.
// Launch with WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9223.
import assert from "node:assert/strict";
import { mkdir } from "node:fs/promises";
import { chromium } from "playwright";

const browser = await chromium.connectOverCDP("http://127.0.0.1:9223");
const page = browser.contexts().flatMap((context) => context.pages())
  .find((candidate) => candidate.url().includes("tauri.localhost"));
assert(page, "Expected the packaged Tauri application page");
await page.locator("#record").waitFor();
const invoke = (command, args) => page.evaluate(
  ({ command, args }) => window.__TAURI_INTERNALS__.invoke(command, args),
  { command, args },
);
const original = await invoke("get_settings");
assert.equal((await invoke("get_detection_state")).automation_inhibited, true, "Launch test app with MEETING_RECORDER_DISABLE_AUTOMATION=1 before changing preferences");
const transcription = await invoke("get_transcription_state");
assert.equal(transcription.inhibited, true, "Launch test app with MEETING_RECORDER_DISABLE_TRANSCRIPTION=1 before changing preferences");
assert.equal(transcription.error, null, "Transcription database and settings should remain readable");
await assert.rejects(invoke("retry_transcription", { recordingId: "00000000-0000-0000-0000-000000000000" }), /disabled/i);
await assert.rejects(invoke("open_transcript", { recordingId: "../outside" }), /unknown|invalid/i);
const originalSourceIds = original.audio_sources.map((source) => source.id);
assert.equal((await invoke("get_recording_state")).active, null, "Do not run smoke checks during a user's recording");
try {
  const state = await invoke("get_app_state");
  assert.equal(state.status, "idle");
  assert.equal(state.recording_supported, true);
  assert.equal(typeof state.detection_supported, "boolean");
  assert.equal(state.appflowy_connected, false);
  assert.equal(state.tray_available, true);
  // Never opens a microphone: explicitly empty selection exercises the guard.
  const before = await invoke("get_recording_state");
  await assert.rejects(invoke("confirm_detected_recording", { approvalId: "expired-test-request" }), /expired|handled/i);
  await assert.rejects(invoke("dismiss_detected_recording", { approvalId: "expired-test-request" }), /expired/i);
  assert.deepEqual(await invoke("get_recording_state"), before, "Invalid approvals never initiate capture");
  await invoke("save_audio_selection", { sourceIds: [] });
  await assert.rejects(invoke("start_recording"), /select|source/i);
  assert.equal((await invoke("get_recording_state")).active, null);
  assert.deepEqual((await invoke("get_recording_state")).recent, before.recent);
  await assert.rejects(invoke("stop_recording"), /No recording/);
  await invoke("save_audio_selection", { sourceIds: originalSourceIds });

  await page.locator('[data-page="settings"]').click();
  await page.locator('[name="auto_record"]').setChecked(!original.auto_record);
  await page.locator('[name="whisper_model"]').selectOption("small");
  await page.getByRole("button", { name: "Save preferences" }).click();
  await page.locator("#toast").filter({ hasText: "Preferences saved locally" }).waitFor();
  const modified = await invoke("get_settings");
  assert.equal(modified.auto_record, !original.auto_record);
  assert.equal(modified.whisper_model, "small");
  await assert.rejects(invoke("save_settings", {
    settings: { ...modified, end_grace_seconds: 0 },
  }), /grace/i);
  assert.deepEqual(await invoke("get_settings"), modified);
  await page.reload();
  await page.locator('[data-page="settings"]').click();
  assert.equal(await page.locator('[name="auto_record"]').isChecked(), !original.auto_record);
  assert.equal(await page.locator('[name="whisper_model"]').inputValue(), "small");
  await page.locator('[data-page="dashboard"]').click();
  await mkdir("artifacts", { recursive: true });
  await page.screenshot({ path: "artifacts/native-dashboard.png", fullPage: true });
  console.log(JSON.stringify({ result: "passed", state, checks: [
    "packaged assets load", "native IPC", "tray created", "start/stop guards",
    "UI settings save", "invalid save preserved state", "webview reload persistence",
  ] }, null, 2));
} finally {
  await invoke("save_audio_selection", { sourceIds: originalSourceIds });
  await invoke("save_settings", { settings: original });
  await page.reload();
  await browser.close();
}
