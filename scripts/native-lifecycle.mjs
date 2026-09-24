// Optional Windows-only lifecycle check. Requires the packaged debug app running
// with a local WebView2 debugging port, as described in native-smoke.mjs.
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { setTimeout as delay } from "node:timers/promises";
import { resolve } from "node:path";
import { chromium } from "playwright";

const executable = resolve(process.env.MEETING_RECORDER_TEST_EXECUTABLE ?? "src-tauri/target/debug/meeting-recorder.exe");
const getPids = () => execFileSync("powershell.exe", ["-NoProfile", "-Command",
  "Get-Process -Name 'meeting-recorder' -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Id; exit 0",
], { encoding: "utf8", windowsHide: true }).trim().split(/\s+/).filter(Boolean);
const connect = async () => {
  const browser = await chromium.connectOverCDP("http://127.0.0.1:9223");
  let page;
  await eventually(() => {
    page = browser.contexts().flatMap((context) => context.pages())
      .find((candidate) => candidate.url().includes("tauri.localhost"));
    return Boolean(page);
  });
  assert(page);
  await page.locator("#record").waitFor();
  return { browser, page };
};
const invoke = (page, command, args) => page.evaluate(
  ({ command, args }) => window.__TAURI_INTERNALS__.invoke(command, args), { command, args },
);
const launch = () => {
  const child = spawn(executable, [], { stdio: "ignore", windowsHide: true,
    env: { ...process.env, MEETING_RECORDER_DISABLE_AUTOMATION: "1", MEETING_RECORDER_DISABLE_TRANSCRIPTION: "1", WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: "--remote-debugging-port=9223" } });
  child.unref();
  return child;
};
const eventually = async (predicate) => {
  for (let attempt = 0; attempt < 40; attempt++) {
    if (await predicate()) return;
    await delay(250);
  }
  throw new Error("Timed out waiting for native lifecycle transition");
};

if (getPids().length === 0) {
  launch();
  await eventually(async () => {
    try { return (await fetch("http://127.0.0.1:9223/json/version")).ok; }
    catch { return false; }
  });
}
let { browser, page } = await connect();
const original = await invoke(page, "get_settings");
assert.equal((await invoke(page, "get_detection_state")).automation_inhibited, true, "Launch test app with MEETING_RECORDER_DISABLE_AUTOMATION=1 before changing preferences");
assert.equal((await invoke(page, "get_transcription_state")).inhibited, true, "Launch test app with MEETING_RECORDER_DISABLE_TRANSCRIPTION=1 before changing preferences");
assert.equal((await invoke(page, "get_diarization_state")).inhibited, true, "Launch test app with MEETING_RECORDER_DISABLE_TRANSCRIPTION=1 before changing preferences");
const originalRecordings = await invoke(page, "get_recording_state");
const originalTranscription = await invoke(page, "get_transcription_state");
const originalDiarization = await invoke(page, "get_diarization_state");
assert.equal(originalRecordings.active, null, "Do not restart during a user's recording");
const testModel = original.whisper_model === "small" ? "base" : "small";
const testAudio = process.argv.includes("--audio");
const originalSourceIds = original.audio_sources?.map((source) => source.id) ?? [];
let expectedSources = original.audio_sources;
if (testAudio) {
  const inventory = await invoke(page, "get_audio_devices");
  const candidates = inventory.devices.filter((device) => device.selectable && device.direction === "input").slice(0, 2);
  assert(candidates.length > 0, "Audio restart check needs at least one native input");
  const selected = await invoke(page, "save_audio_selection", {
    sourceIds: [...new Set([...originalSourceIds, ...candidates.map((device) => device.id)])],
  });
  expectedSources = selected.audio_sources;
}
const [firstPid] = getPids();
assert(firstPid);
execFileSync("powershell.exe", ["-NoProfile", "-Command",
  `(Get-Process -Id ${Number(firstPid)}).CloseMainWindow()`,
], { windowsHide: true });
await delay(400);
assert.deepEqual(getPids(), [firstPid], "Closing window must preserve tray process");
const second = launch();
await eventually(() => second.exitCode !== null);
assert.deepEqual(getPids(), [firstPid], "Second launch must not leave duplicate instances");
assert.equal(await invoke(page, "plugin:window|is_visible", { label: "main" }), true);

await invoke(page, "save_settings", { settings: { ...original, whisper_model: testModel } });
try { await invoke(page, "quit_application"); } catch { /* Window may close before IPC returns. */ }
await browser.close();
await eventually(() => getPids().length === 0);
launch();
await eventually(async () => {
  try { const result = await fetch("http://127.0.0.1:9223/json/version"); return result.ok; }
  catch { return false; }
});
({ browser, page } = await connect());
try {
  const restarted = await invoke(page, "get_settings");
  assert.equal(restarted.whisper_model, testModel);
  const restartedRecordings = await invoke(page, "get_recording_state");
  const restartedTranscription = await invoke(page, "get_transcription_state");
  assert.equal(restartedTranscription.inhibited, true);
  assert.deepEqual(restartedTranscription.jobs, originalTranscription.jobs, "Inhibited restart must not claim or transcribe any audio");
  const restartedDiarization = await invoke(page, "get_diarization_state");
  assert.equal(restartedDiarization.inhibited, true);
  assert.deepEqual(restartedDiarization.jobs, originalDiarization.jobs, "Inhibited restart must not claim speaker jobs");
  assert.equal(restartedRecordings.active, null);
  assert.deepEqual(restartedRecordings.recent, originalRecordings.recent, "Restart must preserve completed history without duplicates");
  if (testAudio) assert.deepEqual(restarted.audio_sources, expectedSources);
  console.log("Passed: close-to-tray, single instance restores window, full process restart preserves settings.");
  console.log("Passed: completed recording history is unchanged after restart; capture did not resume.");
  if (testAudio) console.log("Passed: native input selections persist across full process restart.");
} finally {
  if (testAudio) await invoke(page, "save_audio_selection", { sourceIds: originalSourceIds });
  await invoke(page, "save_settings", { settings: original });
  try { await invoke(page, "quit_application"); } catch { /* Window may close before IPC returns. */ }
  await browser.close();
}
