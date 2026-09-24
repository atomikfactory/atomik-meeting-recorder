import { expect, test } from "@playwright/test";

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const source = { id: "native:mic:input", name: "Desk microphone", direction: "input" };
    const fixture = {
      starts: 0, stops: 0, reads: 0, failStart: false, failStop: false,
      snapshot: { status: "idle", active: null as any, recent: [] as any[], error: null as string | null },
    };
    Object.assign(window, {
      __recordingFixture: fixture,
      __TAURI_INTERNALS__: {
        transformCallback: () => 1,
        invoke: async (command: string) => {
          if (command === "plugin:event|listen") return 1;
          if (command === "get_settings") return { auto_record: true, start_debounce_seconds: 10, end_grace_seconds: 30, whisper_model: "base", audio_sources: [source] };
          if (command === "get_app_state") return { status: "idle", recording_supported: true, detection_supported: false, appflowy_connected: false, recordings_folder: "test", tray_available: true, tray_error: null };
          if (command === "get_audio_devices") return { backend: "Test backend", errors: [], devices: [{ ...source, is_default: true, default_format: "1 channel · 48000 Hz", selectable: true, unavailable_reason: null }] };
          if (command === "get_recording_state") { fixture.reads++; return structuredClone(fixture.snapshot); }
          if (command === "start_recording") {
            fixture.starts++;
            if (fixture.failStart) throw new Error("Microphone permission denied");
            if (fixture.snapshot.active) throw new Error("Already recording");
            fixture.snapshot = { status: "recording", error: null, recent: fixture.snapshot.recent, active: {
              recording_id: "test-recording-1", started_at: Date.now() - 34000, ended_at: null, duration_ms: 34000,
              status: "recording", error: null, transcription_status: "pending", appflowy_status: "pending",
              tracks: [{ source_id: source.id, name: source.name, direction: source.direction, sample_rate: 48000, channels: 1, file_path: "test/track.wav", frames_written: 48000, dropped_samples: 0, error: null }],
            } };
            return structuredClone(fixture.snapshot);
          }
          if (command === "stop_recording") {
            fixture.stops++;
            if (fixture.failStop) throw new Error("Could not finalize recording");
            fixture.snapshot = { status: "idle", error: null, active: null, recent: [{ ...fixture.snapshot.active, status: "recorded", ended_at: Date.now(), duration_ms: 35000 }] };
            return structuredClone(fixture.snapshot);
          }
          throw new Error(`Unexpected command ${command}`);
        },
      },
    });
  });
  await page.goto("/");
  await expect(page.locator("#record")).toBeEnabled();
});

test("manual capture shows timer and sources, then a saved recording", async ({ page }) => {
  await page.locator("#record").click();
  await expect(page.locator("#recording-status")).toContainText(/recording/i);
  await expect(page.locator("#recording-timer")).toContainText(/00:3[4-9]/);
  await expect(page.locator("#record")).toBeDisabled();
  await page.getByRole("button", { name: "Settings", exact: true }).click();
  await expect(page.getByRole("checkbox", { name: /Desk microphone/ })).toBeDisabled();
  await expect(page.getByRole("button", { name: "Save audio sources" })).toBeDisabled();
  await page.getByRole("button", { name: "Dashboard", exact: true }).click();
  await page.locator("#stop-recording").click();
  await expect(page.locator("#record")).toBeEnabled();
  await expect(page.locator("#recent-recordings")).toContainText("Desk microphone");
  await expect(page.locator("#recent-recordings")).toContainText(/recorded|saved/i);
  expect(await page.evaluate(() => [(window as any).__recordingFixture.starts, (window as any).__recordingFixture.stops])).toEqual([1, 1]);
  await page.screenshot({ path: "test-results/recording-saved.png", fullPage: true });
});

test("start and stop failures do not fabricate transitions", async ({ page }) => {
  await page.evaluate(() => { (window as any).__recordingFixture.failStart = true; });
  await page.locator("#record").click();
  await expect(page.locator("#toast")).toContainText("Microphone permission denied");
  await expect(page.locator("#record")).toBeEnabled();
  expect(await page.evaluate(() => (window as any).__recordingFixture.snapshot.active)).toBeNull();
  await page.evaluate(() => { (window as any).__recordingFixture.failStart = false; });
  await page.locator("#record").click();
  await expect(page.locator("#stop-recording")).toBeEnabled();
  await page.evaluate(() => { (window as any).__recordingFixture.failStop = true; });
  await page.locator("#stop-recording").click();
  await expect(page.locator("#toast")).toContainText("Could not finalize recording");
  await expect(page.locator("#record")).toBeDisabled();
  await expect(page.locator("#stop-recording")).toBeEnabled();
});

test("polling preserves settings draft and reports a background device failure safely", async ({ page }) => {
  await page.locator("#record").click();
  await expect(page.locator("#stop-recording")).toBeEnabled();
  await page.getByRole("button", { name: "Settings", exact: true }).click();
  await page.locator('[name="start_debounce_seconds"]').fill("23");
  await page.evaluate(() => {
    const fixture = (window as any).__recordingFixture;
    const message = "<img src=x onerror=alert(1)> Device disconnected; partial audio retained";
    fixture.snapshot = { status: "failed", active: null, error: message, recent: [{ ...fixture.snapshot.active, status: "interrupted", ended_at: Date.now(), error: message }] };
  });
  await expect(page.locator("#recording-error")).toContainText("Device disconnected");
  await expect(page.locator('[name="start_debounce_seconds"]')).toHaveValue("23");
  await page.getByRole("button", { name: "Dashboard", exact: true }).click();
  await expect(page.locator("#recent-recordings")).toContainText(/Audio saved with interruptions/i);
  await expect(page.locator("#recent-recordings img")).toHaveCount(0);
  await expect(page.locator("#record")).toBeEnabled();
});

test("an active session retains Stop while a device error is being handled", async ({ page }) => {
  await page.locator("#record").click();
  await expect(page.locator("#stop-recording")).toBeEnabled();
  await page.evaluate(() => {
    const fixture = (window as any).__recordingFixture;
    fixture.snapshot.status = "failed";
    fixture.snapshot.error = "Device stopped responding";
  });
  await expect(page.locator("#recording-error")).toContainText("Device stopped responding");
  await expect(page.locator("#record")).toBeDisabled();
  await expect(page.locator("#stop-recording")).toBeVisible();
  await expect(page.locator("#stop-recording")).toBeEnabled();
  await page.locator("#stop-recording").click();
  await expect(page.locator("#record")).toBeEnabled();
});
