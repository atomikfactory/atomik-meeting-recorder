import { expect, test } from "@playwright/test";

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const record = { recording_id: "abc", meeting_title: "<b>Team sync</b>", started_at: 1700000000000, ended_at: 1, duration_ms: 1, status: "recorded", tracks: [{ source_id: "mic", name: "Desk mic", direction: "input", sample_rate: 48000, channels: 1, file_path: "", frames_written: 1, dropped_samples: 0, error: null }], error: null, transcription_status: "", appflowy_status: "" };
    const fixture: any = { record, jobs: [], calls: [], deferred: false, resolve: null };
    Object.assign(window, { __library: fixture, __TAURI_INTERNALS__: { transformCallback: () => 1, invoke: async (command: string, args: any) => {
      fixture.calls.push([command, args]);
      if (command === "plugin:event|listen") return 1;
      if (command === "get_settings") return { auto_record: true, start_debounce_seconds: 10, end_grace_seconds: 30, whisper_model: "base", audio_sources: [] };
      if (command === "get_app_state") return { status: "idle", recording_supported: true, detection_supported: false, appflowy_connected: false, recordings_folder: "", tray_available: true, tray_error: null };
      if (command === "get_recording_state") return { status: "idle", active: null, recent: [fixture.record], error: null };
      if (command === "get_transcription_state") return { inhibited: false, model: { key: "base", available: false }, jobs: fixture.jobs, error: null, backend: "cpu (AVX2)" };
      if (command === "get_detection_state") return { supported: false, enabled: false, phase: "unsupported", detail: "", error: null, seconds_remaining: null, automation_inhibited: false };
      if (command === "get_diarization_state") return { enabled: true, inhibited: false, models: [], jobs: [], error: null };
      if (command === "get_audio_devices") return { backend: "", devices: [], errors: [] };
      if (command === "transcribe_recording" && fixture.deferred) return new Promise(resolve => fixture.resolve = resolve);
      if (["open_recording_folder", "open_transcript", "transcribe_recording"].includes(command)) return null;
      throw Error(command);
    } } });
  });
  await page.goto("/");
});

test("history shows escaped meeting title, local timestamp, and folder action", async ({ page }) => {
  await expect(page.locator("#recent-recordings")).toContainText("<b>Team sync</b>");
  await expect(page.locator("#recent-recordings")).toContainText("11/14/2023");
  await expect(page.locator("#recent-recordings b")).toHaveCount(1);
  await page.getByRole("button", { name: "Open folder" }).click();
  expect(await page.evaluate(() => (window as any).__library.calls.find((x: any) => x[0] === "open_recording_folder"))).toEqual(["open_recording_folder", { recordingId: "abc" }]);
  await page.screenshot({ path: "artifacts/recording-library.png", fullPage: true });
});

test("search filters loaded history and preserves focused query", async ({ page }) => {
  const search = page.locator("#history-search");
  await search.fill("no match");
  await expect(page.locator(".recording-row")).toHaveCount(0);
  await search.fill("Team");
  await page.waitForTimeout(1100);
  await expect(search).toHaveValue("Team");
  await expect(search).toBeFocused();
  await expect(page.locator(".recording-row")).toHaveCount(1);
});

test("legacy transcribe is one in-flight ID and re-enables after completion", async ({ page }) => {
  await page.evaluate(() => { (window as any).__library.deferred = true; });
  const button = page.getByRole("button", { name: "Transcribe" });
  await button.click();
  await page.waitForTimeout(1100);
  await expect(button).toBeDisabled();
  await page.evaluate(() => (window as any).__library.resolve(null));
  await expect(button).toBeEnabled();
  expect(await page.evaluate(() => (window as any).__library.calls.filter((x: any) => x[0] === "transcribe_recording"))).toEqual([["transcribe_recording", { recordingId: "abc" }]]);
});

test("completed transcript opens exact ID and has no transcribe action", async ({ page }) => {
  await page.evaluate(() => { (window as any).__library.jobs = [{ recording_id: "abc", status: "completed", progress: 100, error: null, markdown_path: "x" }]; });
  await page.waitForTimeout(1100);
  await page.getByRole("button", { name: "Open transcript" }).click();
  expect(await page.evaluate(() => (window as any).__library.calls.find((x: any) => x[0] === "open_transcript"))).toEqual(["open_transcript", { recordingId: "abc" }]);
  await expect(page.getByRole("button", { name: "Transcribe" })).toHaveCount(0);
});

test("waiting model routes to Settings and focuses the Whisper chooser", async ({ page }) => {
  await page.evaluate(() => { (window as any).__library.jobs = [{ recording_id: "abc", status: "waiting_model", progress: 0, error: null, markdown_path: null, model_key: "base" }]; });
  await page.waitForTimeout(1100);
  await page.getByRole("button", { name: "Choose Whisper model: base" }).click();
  await expect(page.locator("#settings")).not.toHaveClass(/hidden/);
  await expect(page.locator("#choose-model")).toBeFocused();
});
