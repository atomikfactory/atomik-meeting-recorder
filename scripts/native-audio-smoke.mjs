// Phase 2 native verification: enumerate/query/select only; never opens a stream.
// Start the packaged development app with local WebView2 debugging port 9223.
import assert from "node:assert/strict";
import { mkdir } from "node:fs/promises";
import { chromium } from "playwright";

const browser = await chromium.connectOverCDP("http://127.0.0.1:9223");
const page = browser.contexts().flatMap((context) => context.pages())
  .find((candidate) => candidate.url().includes("tauri.localhost"));
assert(page, "Expected packaged app");
await page.locator("#record").waitFor();
const invoke = (command, args) => page.evaluate(
  ({ command, args }) => window.__TAURI_INTERNALS__.invoke(command, args), { command, args },
);
const original = await invoke("get_settings");
assert.equal((await invoke("get_detection_state")).automation_inhibited, true, "Launch test app with MEETING_RECORDER_DISABLE_AUTOMATION=1 before changing preferences");
assert.equal((await invoke("get_transcription_state")).inhibited, true, "Launch test app with MEETING_RECORDER_DISABLE_TRANSCRIPTION=1 before changing preferences");
const originalIds = original.audio_sources.map((source) => source.id);
try {
  const inventory = await invoke("get_audio_devices");
  assert.equal(inventory.backend.toLowerCase(), "wasapi");
  assert.equal(inventory.errors.length, 0, JSON.stringify(inventory.errors));
  const ids = inventory.devices.filter((device) => device.id).map((device) => device.id);
  assert.equal(new Set(ids).size, ids.length, "Direction-qualified IDs must be unique");
  const inputs = inventory.devices.filter((device) => device.direction === "input" && device.selectable);
  const outputs = inventory.devices.filter((device) => device.direction === "output" && device.selectable);
  assert(inputs.length > 0, "Expected native input hardware on this test host");
  assert(outputs.length > 0, "Expected WASAPI output endpoints");
  for (const device of [...inputs, ...outputs]) {
    assert(device.default_format, `Missing default format for ${device.name}`);
  }
  const chosen = [...new Set([...originalIds, ...inputs.slice(0, 2).map((device) => device.id), outputs[0].id])];
  const saved = await invoke("save_audio_selection", { sourceIds: chosen });
  assert.deepEqual(saved.audio_sources.map((source) => source.id), chosen);
  await assert.rejects(invoke("save_audio_selection", { sourceIds: [chosen[0], chosen[0]] }));
  await assert.rejects(invoke("save_audio_selection", { sourceIds: ["not-a-real-native-endpoint"] }));
  assert.deepEqual((await invoke("get_settings")).audio_sources, saved.audio_sources);

  // A stale preferences form cannot erase a newer source selection.
  await invoke("save_settings", { settings: { ...original, audio_sources: [] } });
  assert.deepEqual((await invoke("get_settings")).audio_sources, saved.audio_sources);
  const refreshed = await invoke("get_audio_devices");
  assert.deepEqual(refreshed.devices.filter((device) => device.id).map((device) => device.id).sort(), [...ids].sort());
  await page.reload();
  await page.locator('[data-page="settings"]').click();
  await page.locator("#inventory-backend").filter({ hasText: "Wasapi" }).waitFor();
  assert.equal(await page.locator(".audio-source:checked").count(), chosen.length);
  assert.deepEqual((await invoke("get_settings")).audio_sources, saved.audio_sources);
  await mkdir("artifacts", { recursive: true });
  await page.screenshot({ path: "artifacts/native-audio-settings.png", fullPage: true });
  console.log(JSON.stringify({ result: "passed", backend: inventory.backend,
    inputCount: inputs.length, outputCount: outputs.length,
    devices: inventory.devices.map(({ name, direction, default_format, is_default }) => ({ name, direction, default_format, is_default })),
    checks: ["native enumeration", "unique stable IDs", "multiple source selection", "default format metadata",
      "duplicate/unknown rejection", "stale preferences preserve sources", "refresh stability", "reload persistence"],
  }, null, 2));
} finally {
  await invoke("save_audio_selection", { sourceIds: originalIds });
  await invoke("save_settings", { settings: original });
  await page.reload();
  await browser.close();
}
