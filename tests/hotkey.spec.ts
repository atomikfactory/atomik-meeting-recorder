import { expect, test } from "@playwright/test";

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const f: any = {
      calls: [],
      hotkeyStatus: {
        enabled: true,
        hotkey: "CommandOrControl+Alt+R",
        registered: true,
        error: null,
      },
    };
    Object.assign(window, {
      __hk: f,
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
              hotkey_enabled: true,
              hotkey: "CommandOrControl+Alt+R",
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
          if (c === "get_browser_extension_status") return { connected: false, last_message_seconds_ago: null, registered: false, browsers: [] };
          if (c === "save_settings") return a.settings;
          if (c === "get_hotkey_status") return f.hotkeyStatus;
          throw new Error(`Unexpected test command: ${c}`);
        },
      },
    });
  });
  await page.goto("/");
  await page.getByRole("button", { name: "Settings", exact: true }).click();
});

test("shows the saved shortcut and a registered status by default", async ({ page }) => {
  await expect(page.locator("#hotkey-capture")).toHaveText("Ctrl+Alt+R");
  await expect(page.locator("#hotkey-status")).toContainText("Registered: Ctrl+Alt+R");
});

test("pressing a new combination updates the capture field's display", async ({ page }) => {
  await page.locator("#hotkey-capture").click();
  await expect(page.locator("#hotkey-capture")).toHaveText("Press keys… (Esc to cancel)");
  await page.keyboard.press("Control+Alt+P");
  await expect(page.locator("#hotkey-capture")).toHaveText("Ctrl+Alt+P");
});

test("Escape cancels capture and reverts to the previous value", async ({ page }) => {
  await page.locator("#hotkey-capture").click();
  await page.keyboard.press("Escape");
  await expect(page.locator("#hotkey-capture")).toHaveText("Ctrl+Alt+R");
});

test("Backspace clears the field to none set", async ({ page }) => {
  await page.locator("#hotkey-capture").click();
  await page.keyboard.press("Backspace");
  await expect(page.locator("#hotkey-capture")).toHaveText("None set");
});

test("a modifier-only press keeps listening instead of accepting an incomplete combo", async ({ page }) => {
  await page.locator("#hotkey-capture").click();
  await page.keyboard.down("Control");
  await expect(page.locator("#hotkey-capture")).toHaveText("Press keys… (Esc to cancel)");
  await page.keyboard.up("Control");
  await page.keyboard.press("Escape");
});

test("a captured shortcut survives several poll cycles before saving", async ({ page }) => {
  await page.locator("#hotkey-capture").click();
  await page.keyboard.press("Control+Alt+P");
  await expect(page.locator("#hotkey-capture")).toHaveText("Ctrl+Alt+P");
  // The 1s hotkey-status poll (and the app's other 1-5s polls) must not
  // rebuild the settings form and wipe this unsaved capture -- see
  // TESTING LESSON in the project instructions.
  await page.waitForTimeout(4000);
  await expect(page.locator("#hotkey-capture")).toHaveText("Ctrl+Alt+P");
});

test("disabling the shortcut and saving posts hotkey_enabled=false", async ({ page }) => {
  await page.locator('[name="hotkey_enabled"]').uncheck();
  await page.getByRole("button", { name: "Save preferences" }).click();
  const saved = await page.evaluate(() => (window as any).__hk.calls.find((x: any[]) => x[0] === "save_settings")[1].settings);
  expect(saved.hotkey_enabled).toBe(false);
  expect(saved.hotkey).toBe("CommandOrControl+Alt+R");
});

test("saving a freshly captured shortcut posts it verbatim", async ({ page }) => {
  await page.locator("#hotkey-capture").click();
  await page.keyboard.press("Control+Alt+P");
  await page.getByRole("button", { name: "Save preferences" }).click();
  const saved = await page.evaluate(() => (window as any).__hk.calls.find((x: any[]) => x[0] === "save_settings")[1].settings);
  expect(saved.hotkey).toBe("Control+Alt+P");
  expect(saved.hotkey_enabled).toBe(true);
});

test("a registration failure reported by the backend is rendered as text", async ({ page }) => {
  await page.evaluate(() => {
    (window as any).__hk.hotkeyStatus = {
      enabled: true,
      hotkey: "CommandOrControl+Alt+R",
      registered: false,
      error: "Could not register CommandOrControl+Alt+R: os error. Another application may already be using this shortcut.",
    };
  });
  await page.waitForTimeout(1100); // the 1s hotkey-status poll
  await expect(page.locator("#hotkey-status")).toContainText("Another application may already be using this shortcut");
});

test("an invalid combination's error is rendered as text, not executed", async ({ page }) => {
  await page.evaluate(() => {
    (window as any).__hk.hotkeyStatus = {
      enabled: true,
      hotkey: "Alt+F4",
      registered: false,
      error: "<img src=x onerror=alert(1)> is reserved by the operating system and cannot be used here.",
    };
  });
  await page.waitForTimeout(1100);
  await expect(page.locator("img[src=x]")).toHaveCount(0);
  await expect(page.locator("#hotkey-status")).toContainText("is reserved by the operating system");
});
