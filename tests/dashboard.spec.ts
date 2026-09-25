import { expect, test } from "@playwright/test";

test("settings access failure preserves data and offers recovery", async ({
  page,
}) => {
  await page.addInitScript(() => {
    let settingsReads = 0;
    const internals = {
      transformCallback: () => 1,
      invoke: async (command: string) => {
        if (command === "plugin:event|listen") return 1;
        if (command === "get_settings") {
          if (settingsReads++ === 0)
            throw new Error("Settings file is unavailable");
          return {
            auto_record: true,
            start_debounce_seconds: 18,
            end_grace_seconds: 45,
            whisper_model: "base",
          };
        }
        if (command === "get_app_state")
          return {
            status: "idle",
            recording_supported: false,
            detection_supported: false,
            appflowy_connected: false,
            recordings_folder: "test",
            tray_available: false,
            tray_error: "<img src=x onerror=alert(1)> Tray unavailable",
          };
        if (command === "get_recording_state")
          return { status: "idle", active: null, recent: [], error: null };
        if (command === "get_audio_devices") return { backend: "test", errors: [], devices: [{ id: "mic-1", name: "Desk mic", direction: "input", is_default: true, default_format: "2 channels · 48000 Hz", selectable: true, unavailable_reason: null }] };
        if (command === "get_transcription_state") return { inhibited: false, model: { key: "base", available: false }, jobs: [], error: null, backend: "cpu (AVX2)" };
        if (command === "get_diarization_state") return { enabled: true, inhibited: false, models: [], jobs: [], error: null };
        if (command === "get_summary_state") return { enabled: true, inhibited: false, jobs: [], error: null };
        if (command === "get_compression_state") return { enabled: true, inhibited: false, jobs: [], error: null };
        if (command === "audio_storage_summary") return { total_bytes: 0, total_original_bytes: 0, recordings: [] };
        if (command === "save_settings") throw new Error("Permission denied");
        throw new Error(`Unexpected test command: ${command}`);
      },
    };
    Object.assign(window, { __TAURI_INTERNALS__: internals });
  });
  await page.goto("/");
  await expect(page.locator("#toast")).toContainText(
    "Settings file is unavailable",
  );
  await expect(page.locator("#record")).toBeDisabled();
  await expect(page.locator("#quit")).toBeEnabled();
  await page.getByRole("button", { name: "Retry desktop connection" }).click();
  await expect(page.locator("#retry")).toHaveCount(0);
  await expect(page.locator(".notice img")).toHaveCount(0);
  await expect(page.locator(".notice")).toContainText(
    "<img src=x onerror=alert(1)>",
  );
  await page.getByRole("button", { name: "Settings", exact: true }).click();
  await expect(page.locator('[name="start_debounce_seconds"]')).toHaveValue(
    "18",
  );
  await page.locator('[name="start_debounce_seconds"]').fill("23");
  await expect(page.locator('[data-for="start_debounce_seconds"]')).toHaveText(
    "23 seconds",
  );
  await page.getByRole("button", { name: "Save preferences" }).click();
  await expect(page.locator("#toast")).toContainText("Permission denied");
  await expect(page.locator('[name="start_debounce_seconds"]')).toHaveValue(
    "23",
  );
});

test("audio inventory supports multiple source drafts and saves independently", async ({ page }) => {
  await page.addInitScript(() => {
    const calls: string[] = [];
    Object.assign(window, { __TAURI_INTERNALS__: { transformCallback: () => 1, invoke: async (command: string) => {
      calls.push(command); if (command === "plugin:event|listen") return 1;
      if (command === "get_settings") return { auto_record:true,start_debounce_seconds:10,end_grace_seconds:30,whisper_model:"base",audio_sources:[] };
      if (command === "get_app_state") return { status:"idle",recording_supported:false,detection_supported:false,appflowy_connected:false,recordings_folder:"test",tray_available:false,tray_error:null };
      if (command === "get_recording_state") return { status:"idle",active:null,recent:[],error:null };
      if (command === "get_audio_devices") return { backend:"test",errors:[],devices:[{id:"one",name:"Input one",direction:"input",is_default:true,default_format:"1 channels · 48000 Hz",selectable:true,unavailable_reason:null},{id:"two",name:"Input two",direction:"input",is_default:false,default_format:"2 channels · 48000 Hz",selectable:true,unavailable_reason:null}] };
      if (command === "save_audio_selection") return { auto_record:true,start_debounce_seconds:10,end_grace_seconds:30,whisper_model:"base",audio_sources:[{id:"one",name:"Input one",direction:"input"},{id:"two",name:"Input two",direction:"input"}] };
      if (command === "save_settings") return { auto_record:true,start_debounce_seconds:10,end_grace_seconds:30,whisper_model:"base",audio_sources:[] };
      throw new Error(`Unexpected ${command}`);
    } } });
  });
  await page.goto("/"); await page.getByRole("button", { name: "Settings", exact: true }).click();
  await expect(page.getByText("Input one")).toBeVisible();
  await page.locator(".audio-source").nth(0).check(); await page.locator(".audio-source").nth(1).check();
  await page.getByRole("button", { name: "Save audio sources" }).click();
  await expect(page.locator("#toast")).toContainText("Audio sources saved");
});

test("browser preview is explicit and cannot mutate", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByText("Browser preview only")).toBeVisible();
  await expect(
    page.getByRole("button", { name: /Start recording/ }),
  ).toBeDisabled();
  await expect(
    page.getByRole("button", { name: "Open recordings folder" }),
  ).toBeDisabled();
  await page.getByRole("button", { name: "Settings" }).click();
  await expect(page.getByRole("heading", { name: "Settings" })).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Save preferences" }),
  ).toBeDisabled();
});

test("dashboard renders capability truth and screenshots", async ({ page }) => {
  await page.setViewportSize({ width: 1080, height: 760 });
  await page.goto("/");
  await expect(
    page.locator("#audio-summary"),
  ).toBeVisible();
  await expect(page.locator(".capabilities").getByText("Local transcription", { exact: true })).toBeVisible();
  await expect(
    page.locator(".capabilities").getByText("Off", { exact: true }),
  ).toBeVisible();
  await page.screenshot({
    path: "test-results/dashboard-1080x760.png",
    fullPage: true,
  });
  await page.setViewportSize({ width: 780, height: 760 });
  expect(
    await page
      .locator("body")
      .evaluate((body) => body.scrollWidth <= window.innerWidth),
  ).toBeTruthy();
  await page.screenshot({
    path: "test-results/dashboard-780x760.png",
    fullPage: true,
  });
});
