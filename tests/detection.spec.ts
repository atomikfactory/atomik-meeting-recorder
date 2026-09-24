import { expect, test } from "@playwright/test";

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const source = { id: "mic", name: "Desk microphone", direction: "input" };
    const fixture = {
      fail: false,
      starts: 0,
      approvals: [] as Array<{ command: string; approvalId: string }>,
      staleApproval: false,
      detection: { supported: true, enabled: true, phase: "waiting", provider: null as string | null, detail: "Waiting for meeting activity. No audio is being captured.", error: null as string | null, seconds_remaining: null as number | null, automation_inhibited: false, approval_id: null as string | null },
    };
    Object.assign(window, {
      __detectionFixture: fixture,
      __TAURI_INTERNALS__: {
        transformCallback: () => 1,
        invoke: async (command: string, args?: { approvalId?: string }) => {
          if (command === "plugin:event|listen") return 1;
          if (command === "get_settings") return { auto_record: true, start_debounce_seconds: 10, end_grace_seconds: 30, whisper_model: "base", audio_sources: [source] };
          if (command === "get_app_state") return { status: "detecting", recording_supported: true, detection_supported: true, appflowy_connected: false, recordings_folder: "test", tray_available: true, tray_error: null };
          if (command === "get_audio_devices") return { backend: "Test", errors: [], devices: [{ ...source, is_default: true, default_format: "48000 Hz", selectable: true, unavailable_reason: null }] };
          if (command === "get_recording_state") return { status: "idle", active: null, recent: [], error: null };
          if (command === "get_transcription_state") return { inhibited: false, model: { key: "base", available: false }, jobs: [], error: null };
          if (command === "get_detection_state") {
            if (fixture.fail) throw new Error("Detector connection unavailable");
            return structuredClone(fixture.detection);
          }
          if (command === "start_recording") { fixture.starts++; throw new Error("Test must not initiate capture"); }
          if (command === "confirm_detected_recording" || command === "dismiss_detected_recording") {
            const approvalId = args?.approvalId;
            if (!approvalId) throw new Error("Approval token required");
            fixture.approvals.push({ command, approvalId });
            if (fixture.staleApproval) throw new Error("This meeting approval has expired.");
            if (command === "dismiss_detected_recording") Object.assign(fixture.detection, { phase: "suppressed", detail: "Automatic recording paused for this meeting.", approval_id: null });
            return;
          }
          throw new Error(`Unexpected command ${command}`);
        },
      },
    });
  });
  await page.goto("/");
});

test("detection countdown and grace update without starting capture in the webview", async ({ page }) => {
  await expect(page.locator("#detection-status")).toContainText(/waiting/i);
  await page.evaluate(() => Object.assign((window as any).__detectionFixture.detection, { phase: "confirming", provider: "google_meet_chrome", detail: "Google Meet activity detected", seconds_remaining: 8 }));
  await expect(page.locator("#detection-status")).toContainText(/confirm/i);
  await expect(page.locator("#detection-countdown")).toContainText("8");
  await page.evaluate(() => Object.assign((window as any).__detectionFixture.detection, { phase: "grace", detail: "Waiting before ending the automatic recording", seconds_remaining: 22 }));
  await expect(page.locator("#detection-countdown")).toContainText("22");
  await expect(page.locator("#detection-detail")).toContainText("Waiting before ending");
  expect(await page.evaluate(() => (window as any).__detectionFixture.starts)).toBe(0);
});

test("detection polling preserves drafts and renders suppression errors as text", async ({ page }) => {
  await expect(page.locator("#detection-status")).toContainText(/waiting/i);
  await page.getByRole("button", { name: "Settings", exact: true }).click();
  await page.locator('[name="start_debounce_seconds"]').fill("23");
  await page.evaluate(() => Object.assign((window as any).__detectionFixture.detection, { phase: "suppressed", detail: "Waiting for this meeting to end before automatic recording can resume", error: "<img src=x onerror=alert(1)> Device unavailable" }));
  await expect(page.locator("#detection-error")).toContainText("Device unavailable");
  await expect(page.locator('[name="start_debounce_seconds"]')).toHaveValue("23");
  await page.getByRole("button", { name: "Dashboard", exact: true }).click();
  await expect(page.locator("#detection-status")).toContainText(/paused|suppressed/i);
  await expect(page.locator("#detection-error img")).toHaveCount(0);
  await expect(page.locator("#record")).toBeEnabled();
});

test("unsupported signals and polling failures leave manual recording available", async ({ page }) => {
  await expect(page.locator("#detection-status")).toContainText(/waiting/i);
  await page.evaluate(() => Object.assign((window as any).__detectionFixture.detection, { supported: false, phase: "unsupported", detail: "Use manual recording on this platform." }));
  await expect(page.locator("#detection-status")).toContainText(/unavailable|unsupported/i);
  await expect(page.locator("#record")).toBeEnabled();
  await page.evaluate(() => { (window as any).__detectionFixture.fail = true; });
  await expect(page.locator("#detection-error")).toContainText("Detector connection unavailable");
  await expect(page.locator("#record")).toBeEnabled();
  await page.evaluate(() => { const f = (window as any).__detectionFixture; f.fail = false; Object.assign(f.detection, { supported: true, phase: "waiting", detail: "Waiting for meeting activity", error: null }); });
  await expect(page.locator("#detection-status")).toContainText(/waiting/i);
  await expect(page.locator("#detection-error")).toBeEmpty();
});

test("detected recordings require an explicit tokenized approval before any capture command", async ({ page }) => {
  await page.evaluate(() => Object.assign((window as any).__detectionFixture.detection, {
    phase: "awaiting_approval", provider: "slack", detail: "Slack meeting activity detected", approval_id: "approval-42",
  }));
  await expect(page.getByRole("button", { name: "Record this meeting" })).toBeEnabled();
  expect(await page.evaluate(() => (window as any).__detectionFixture.approvals)).toEqual([]);
  expect(await page.evaluate(() => (window as any).__detectionFixture.starts)).toBe(0);
  await page.getByRole("button", { name: "Record this meeting" }).click();
  await expect.poll(() => page.evaluate(() => (window as any).__detectionFixture.approvals)).toEqual([
    { command: "confirm_detected_recording", approvalId: "approval-42" },
  ]);
  expect(await page.evaluate(() => (window as any).__detectionFixture.starts)).toBe(0);
});

test("dismissal uses its approval token and stale approval errors do not fabricate a recording", async ({ page }) => {
  await page.evaluate(() => Object.assign((window as any).__detectionFixture.detection, {
    phase: "awaiting_approval", provider: "google_meet_chrome", detail: "Google Meet activity detected", approval_id: "dismiss-7",
  }));
  await page.getByRole("button", { name: "Not this meeting" }).click();
  await expect.poll(() => page.evaluate(() => (window as any).__detectionFixture.approvals)).toEqual([
    { command: "dismiss_detected_recording", approvalId: "dismiss-7" },
  ]);
  await expect(page.locator("#detection-status")).toContainText(/paused|suppressed/i);
  expect(await page.evaluate(() => (window as any).__detectionFixture.starts)).toBe(0);

  await page.evaluate(() => {
    const fixture = (window as any).__detectionFixture;
    fixture.staleApproval = true;
    Object.assign(fixture.detection, { phase: "awaiting_approval", provider: "slack", approval_id: "expired-9", error: null });
  });
  await page.getByRole("button", { name: "Record this meeting" }).click();
  await expect(page.locator("#detection-error")).toContainText("approval has expired");
  await expect(page.locator("#recording-status")).toContainText(/ready/i);
  expect(await page.evaluate(() => (window as any).__detectionFixture.starts)).toBe(0);
});
