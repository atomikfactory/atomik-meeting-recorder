import { expect, test } from "@playwright/test";

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const f: any = {
      calls: [],
      status: {
        connected: false,
        last_message_seconds_ago: null,
        registered: false,
        browsers: [],
      },
    };
    Object.assign(window, {
      __ext: f,
      __TAURI_INTERNALS__: {
        transformCallback: () => 1,
        invoke: async (c: string, a: any) => {
          f.calls.push([c, a]);
          if (c === "plugin:event|listen") return 1;
          if (c === "get_settings")
            return {
              auto_record: true,
              start_debounce_seconds: 10,
              end_grace_seconds: 30,
              whisper_model: "base",
              diarization_enabled: true,
              audio_sources: [],
            };
          if (c === "get_app_state")
            return {
              status: "idle",
              recording_supported: true,
              detection_supported: false,
              appflowy_connected: false,
              recordings_folder: "test",
              tray_available: true,
              tray_error: null,
            };
          if (c === "get_recording_state")
            return { status: "idle", active: null, recent: [], error: null };
          if (c === "get_detection_state")
            return {
              supported: false,
              enabled: false,
              phase: "unsupported",
              detail: "",
              provider: null,
              error: null,
              seconds_remaining: null,
              automation_inhibited: false,
            };
          if (c === "get_transcription_state")
            return { inhibited: false, model: { key: "base", available: false }, jobs: [], error: null, backend: "cpu (AVX2)" };
          if (c === "get_diarization_state") return { enabled: true, inhibited: false, models: [], jobs: [], error: null };
          if (c === "get_summary_state") return { enabled: true, inhibited: false, jobs: [], error: null };
          if (c === "get_compression_state") return { enabled: true, inhibited: false, jobs: [], error: null };
          if (c === "audio_storage_summary") return { total_bytes: 0, total_original_bytes: 0, recordings: [] };
          if (c === "get_audio_devices") return { backend: "test", devices: [], errors: [] };
          if (c === "save_settings") return a.settings;
          if (c === "get_browser_extension_status") return f.status;
          if (c === "connect_browser_extension") {
            if (f.connectResult === "error") throw new Error("Could not write the host manifest.");
            f.status = { connected: false, last_message_seconds_ago: null, registered: true, browsers: ["Chrome", "Edge", "Firefox"] };
            return f.status;
          }
          if (c === "disconnect_browser_extension") {
            f.status = { connected: false, last_message_seconds_ago: null, registered: false, browsers: [] };
            return f.status;
          }
          if (c === "open_browser_extension_folder") return null;
          throw new Error(`Unexpected test command: ${c}`);
        },
      },
    });
  });
  await page.goto("/");
  await page.getByRole("button", { name: "Settings", exact: true }).click();
});

test("shows not-connected status and a disabled Disconnect button by default", async ({ page }) => {
  await expect(page.locator("#browser-extension-status")).toContainText("Not connected");
  await expect(page.locator("#disconnect-browser-extension")).toBeDisabled();
  await expect(page.locator("#connect-browser-extension")).toBeEnabled();
});

test("connecting registers the host and enables Disconnect", async ({ page }) => {
  await page.locator("#connect-browser-extension").click();
  await expect(page.locator("#browser-extension-status")).toContainText("Registered for Chrome, Edge, Firefox");
  await expect(page.locator("#browser-extension-status")).toContainText("Waiting for the browser extension");
  await expect(page.locator("#disconnect-browser-extension")).toBeEnabled();
  const calls = await page.evaluate(() => (window as any).__ext.calls.map((c: any[]) => c[0]));
  expect(calls).toContain("connect_browser_extension");
});

test("a connect failure is shown as text, not a fabricated success", async ({ page }) => {
  await page.evaluate(() => {
    (window as any).__ext.connectResult = "error";
  });
  await page.locator("#connect-browser-extension").click();
  await expect(page.locator("#toast")).toContainText("Could not write the host manifest.");
  await expect(page.locator("#browser-extension-status")).toContainText("Not connected");
  await expect(page.locator(".notice img")).toHaveCount(0);
});

test("a fresh heartbeat shows as connected with recency", async ({ page }) => {
  await page.evaluate(() => {
    (window as any).__ext.status = {
      connected: true,
      last_message_seconds_ago: 3,
      registered: true,
      browsers: ["Chrome"],
    };
  });
  await page.waitForTimeout(5100); // the 5s poll interval
  await expect(page.locator("#browser-extension-status")).toContainText("Connected via Chrome");
  await expect(page.locator("#browser-extension-status")).toContainText("last message 3s ago");
});

test("disconnect unregisters the host and disables itself", async ({ page }) => {
  await page.locator("#connect-browser-extension").click();
  await expect(page.locator("#disconnect-browser-extension")).toBeEnabled();
  await page.locator("#disconnect-browser-extension").click();
  await expect(page.locator("#browser-extension-status")).toContainText("Not connected");
  await expect(page.locator("#disconnect-browser-extension")).toBeDisabled();
});

test("open extension folder invokes the dedicated command", async ({ page }) => {
  await page.locator("#open-browser-extension-folder").click();
  const calls = await page.evaluate(() => (window as any).__ext.calls.map((c: any[]) => c[0]));
  expect(calls).toContain("open_browser_extension_folder");
});
