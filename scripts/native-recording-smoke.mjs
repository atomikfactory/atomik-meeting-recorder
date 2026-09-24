// Explicitly consented Windows hardware check: ONE five-second recording.
// Never plays, transcribes, uploads, deletes, or reads the audio payload.
import assert from "node:assert/strict";
import { mkdir, open, stat } from "node:fs/promises";
import { setTimeout as delay } from "node:timers/promises";
import { chromium } from "playwright";

assert(process.argv.includes("--allow-capture"), "Requires explicit user consent and --allow-capture");
const browser = await chromium.connectOverCDP("http://127.0.0.1:9223");
const page = browser.contexts().flatMap((context) => context.pages()).find((item) => item.url().includes("tauri.localhost"));
assert(page, "Expected packaged app");
await page.locator("#record").waitFor();
const invoke = (command, args) => page.evaluate(({ command, args }) => window.__TAURI_INTERNALS__.invoke(command, args), { command, args });
const original = await invoke("get_settings");
assert.equal((await invoke("get_detection_state")).automation_inhibited, true, "Launch test app with MEETING_RECORDER_DISABLE_AUTOMATION=1 so consent covers only the bounded manual check");
assert.equal((await invoke("get_transcription_state")).inhibited, true, "Launch test app with MEETING_RECORDER_DISABLE_TRANSCRIPTION=1; capture consent does not authorize transcription");
assert.equal((await invoke("get_recording_state")).active, null, "Never interfere with an existing recording");
let started = false;
let recordingId;
try {
  const inventory = await invoke("get_audio_devices");
  const mic = inventory.devices.find((device) => device.direction === "input" && device.selectable && /C922.*Webcam/i.test(device.name));
  const output = inventory.devices.find((device) => device.direction === "output" && device.selectable && device.is_default);
  assert(mic && output, "Only the consented webcam microphone and default output may be selected");
  await invoke("save_audio_selection", { sourceIds: [mic.id, output.id] });
  const captureStarted = Date.now();
  await invoke("start_recording");
  started = true;
  // Keep the capture bounded even if a later UI assertion fails.
  try {
    const active = await invoke("get_recording_state");
    assert.equal(active.status, "recording");
    assert(active.active);
    recordingId = active.active.recording_id;
    assert.equal(active.active.tracks.length, 2);
    await assert.rejects(invoke("start_recording"), /already|progress|active/i);
    await delay(Math.max(0, 5000 - (Date.now() - captureStarted)));
  } finally {
    if ((await invoke("get_recording_state")).active) await invoke("stop_recording");
    started = false;
  }
  const stopped = await invoke("get_recording_state");
  assert.equal(stopped.active, null);
  const record = stopped.recent.find((item) => item.recording_id === recordingId);
  assert(record, "Stable ID appears in recent history");
  assert.equal(stopped.recent.filter((item) => item.recording_id === recordingId).length, 1);
  assert(record.ended_at !== null);
  const headers = [];
  for (const track of record.tracks) {
    const info = await stat(track.file_path);
    const handle = await open(track.file_path, "r");
    const header = Buffer.alloc(44);
    try { await handle.read(header, 0, 44, 0); } finally { await handle.close(); }
    assert.equal(header.toString("ascii", 0, 4), "RIFF");
    assert.equal(header.toString("ascii", 8, 12), "WAVE");
    assert.equal(header.readUInt32LE(4) + 8, info.size, "Final RIFF header matches file length");
    if (track.source_id === mic.id) assert(track.frames_written > 0, "Microphone must deliver actual frames");
    headers.push({ name: track.name, file_path: track.file_path, bytes: info.size, sample_rate: track.sample_rate, channels: track.channels, frames_written: track.frames_written, dropped_samples: track.dropped_samples, error: track.error });
  }
  await page.reload();
  await page.locator("#record").waitFor();
  const reloaded = await invoke("get_recording_state");
  assert.equal(reloaded.recent.filter((item) => item.recording_id === recordingId).length, 1);
  await mkdir("artifacts", { recursive: true });
  await page.screenshot({ path: "artifacts/native-recording-saved.png", fullPage: true });
  console.log(JSON.stringify({ recording_id: recordingId, status: record.status, duration_ms: record.duration_ms, error: record.error, tracks: headers, checks: ["real two-source capture", "duplicate start rejected", "stop finalized WAV headers", "stable recording ID after reload"], privacy: "Only WAV headers inspected. Audio retained locally; never played or uploaded." }, null, 2));
  assert(record.duration_ms >= 4000 && record.duration_ms <= 6500, "Expected the approved approximately five-second session, not an early automatic stop");
} finally {
  if (started) { try { await invoke("stop_recording"); } catch { /* Reported by the main check; retain recoverable files. */ } }
  await invoke("save_audio_selection", { sourceIds: original.audio_sources.map((source) => source.id) });
  await browser.close();
}
