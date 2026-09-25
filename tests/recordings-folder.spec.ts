import { expect, test } from "@playwright/test";

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const f: any = {
      calls: [],
      recordingsFolder: null as string | null,
      currentFolder: "C:\\Users\\test\\AppData\\Roaming\\meetingrecorder\\recordings\\meetings",
      chooseResult: undefined as any,
      moveResult: { results: [] },
    };
    Object.assign(window, {
      __folder: f,
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
              recordings_folder: f.recordingsFolder,
            };
          if (c === "get_app_state")
            return {
              status: "idle",
              recording_supported: true,
              detection_supported: false,
              appflowy_connected: false,
              recordings_folder: f.currentFolder,
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
          if (c === "choose_recordings_folder") {
            if (f.chooseResult === "error") throw new Error("The chosen folder is not writable.");
            if (f.chooseResult === null) return null; // user cancelled
            f.recordingsFolder = "D:\\Meetings";
            f.currentFolder = "D:\\Meetings\\meetings";
            return {
              auto_record: true,
              start_debounce_seconds: 10,
              end_grace_seconds: 30,
              whisper_model: "base",
              diarization_enabled: true,
              audio_sources: [],
              recordings_folder: f.recordingsFolder,
            };
          }
          if (c === "reset_recordings_folder") {
            f.recordingsFolder = null;
            f.currentFolder = "C:\\Users\\test\\AppData\\Roaming\\meetingrecorder\\recordings\\meetings";
            return {
              auto_record: true,
              start_debounce_seconds: 10,
              end_grace_seconds: 30,
              whisper_model: "base",
              diarization_enabled: true,
              audio_sources: [],
              recordings_folder: null,
            };
          }
          if (c === "open_recordings_folder") return null;
          if (c === "move_recordings_to_configured_folder") {
            if (f.moveResult === "error") throw new Error("Stop the active recording before moving recordings.");
            return f.moveResult;
          }
          throw new Error(`Unexpected test command: ${c}`);
        },
      },
    });
  });
  await page.goto("/");
  await page.getByRole("button", { name: "Settings", exact: true }).click();
});

test("shows the current recordings folder and disables reset by default", async ({ page }) => {
  await expect(page.locator("#recordings-folder-path")).toHaveText(
    "C:\\Users\\test\\AppData\\Roaming\\meetingrecorder\\recordings\\meetings",
  );
  await expect(page.locator("#reset-recordings-folder")).toBeDisabled();
});

test("choosing a folder updates the displayed path and settings", async ({ page }) => {
  await page.locator("#choose-recordings-folder").click();
  await expect(page.locator("#recordings-folder-status")).toContainText("updated");
  await expect(page.locator("#recordings-folder-path")).toHaveText("D:\\Meetings\\meetings");
  await expect(page.locator("#reset-recordings-folder")).toBeEnabled();
});

test("cancelling the folder picker makes no visible change", async ({ page }) => {
  await page.evaluate(() => {
    (window as any).__folder.chooseResult = null;
  });
  const before = await page.locator("#recordings-folder-path").textContent();
  await page.locator("#choose-recordings-folder").click();
  await expect(page.locator("#recordings-folder-path")).toHaveText(before ?? "");
  await expect(page.locator("#recordings-folder-status")).toHaveText("");
});

test("a validation failure is shown as text, not a fabricated success", async ({ page }) => {
  await page.evaluate(() => {
    (window as any).__folder.chooseResult = "error";
  });
  await page.locator("#choose-recordings-folder").click();
  await expect(page.locator("#recordings-folder-status")).toContainText("not writable");
  await expect(page.locator(".notice img")).toHaveCount(0);
});

test("reset returns to the default location", async ({ page }) => {
  await page.locator("#choose-recordings-folder").click();
  await expect(page.locator("#reset-recordings-folder")).toBeEnabled();
  await page.locator("#reset-recordings-folder").click();
  await expect(page.locator("#recordings-folder-status")).toContainText("reset to the default");
  await expect(page.locator("#recordings-folder-path")).toHaveText(
    "C:\\Users\\test\\AppData\\Roaming\\meetingrecorder\\recordings\\meetings",
  );
  await expect(page.locator("#reset-recordings-folder")).toBeDisabled();
});

test("open folder invokes the dedicated command", async ({ page }) => {
  await page.locator("#open-recordings-folder-btn").click();
  const calls = await page.evaluate(() => (window as any).__folder.calls.map((c: any[]) => c[0]));
  expect(calls).toContain("open_recordings_folder");
});

test("moving recordings reports a summary including failures as text", async ({ page }) => {
  await page.evaluate(() => {
    (window as any).__folder.moveResult = {
      results: [
        { recording_id: "aaaa", moved: true, error: null },
        { recording_id: "bbbb", moved: false, error: "recording directory missing" },
      ],
    };
  });
  await page.locator("#move-recordings").click();
  await expect(page.locator("#recordings-folder-status")).toContainText("Moved 1 recording");
  await expect(page.locator("#recordings-folder-status")).toContainText("1 failed");
  await expect(page.locator("#recordings-folder-status")).toContainText("recording directory missing");
});

test("a refusal while recording is active is shown as text", async ({ page }) => {
  await page.evaluate(() => {
    (window as any).__folder.moveResult = "error";
  });
  await page.locator("#move-recordings").click();
  await expect(page.locator("#recordings-folder-status")).toContainText(
    "Stop the active recording before moving recordings.",
  );
});
