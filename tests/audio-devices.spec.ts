import { expect, test } from "@playwright/test";

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const one = { id: "native:one:input", name: "Desk microphone", direction: "input", is_default: true, default_format: "1 channel · 48000 Hz", selectable: true, unavailable_reason: null };
    const two = { ...one, id: "native:two:input", name: "USB interface", is_default: false, default_format: "2 channels · 96000 Hz" };
    const output = { ...one, id: "native:output:output", name: "System speakers", direction: "output", is_default: false, selectable: false, unavailable_reason: "Use a supported system-audio adapter." };
    const unusual = { ...one, id: 'native:id"with"quotes:input', name: "<img src=x onerror=alert(1)>", is_default: false, selectable: false, unavailable_reason: "<b>Format unavailable</b>" };
    const source = ({ id, name, direction }: typeof one) => ({ id, name, direction });
    const fixture = {
      mode: "connected", failSave: false, reads: 0, sourceSaves: [] as string[][],
      saved: { auto_record: true, start_debounce_seconds: 10, end_grace_seconds: 30, whisper_model: "base", audio_sources: [source(one)] },
    };
    Object.assign(window, {
      __audioFixture: fixture,
      __TAURI_INTERNALS__: {
        transformCallback: () => 1,
        invoke: async (command: string, args: Record<string, any>) => {
          if (command === "plugin:event|listen") return 1;
          if (command === "get_settings") return structuredClone(fixture.saved);
          if (command === "get_recording_state") return { status: "idle", active: null, recent: [], error: null };
          if (command === "get_app_state") return { status: "idle", recording_supported: false, detection_supported: false, appflowy_connected: false, recordings_folder: "test", tray_available: true, tray_error: null };
          if (command === "get_audio_devices") {
            fixture.reads++;
            return {
              backend: "Test backend",
              devices: fixture.mode === "failed" ? [] : [one, ...(fixture.mode === "missing" ? [] : [two]), output, unusual],
              errors: fixture.mode === "failed" ? ["Audio service is unavailable"] : [],
            };
          }
          if (command === "save_audio_selection") {
            fixture.sourceSaves.push([...args.sourceIds]);
            if (fixture.failSave) throw new Error("Selection could not be saved");
            fixture.saved.audio_sources = args.sourceIds.map((id: string) => {
              const device = [one, two].find((item) => item.id === id);
              if (!device) throw new Error("Unknown source in test");
              return source(device);
            });
            return structuredClone(fixture.saved);
          }
          if (command === "save_settings") {
            fixture.saved = { ...args.settings, audio_sources: fixture.saved.audio_sources };
            return structuredClone(fixture.saved);
          }
          throw new Error(`Unexpected command: ${command}`);
        },
      },
    });
  });
  await page.goto("/");
  await page.getByRole("button", { name: "Settings", exact: true }).click();
  await expect(page.getByRole("checkbox", { name: /USB interface/ })).toBeVisible();
});

test("disconnect and failed enumeration preserve drafts, labels and focus", async ({ page }) => {
  const microphone = page.getByRole("checkbox", { name: /Desk microphone/ });
  const usb = page.getByRole("checkbox", { name: /USB interface/ });
  await expect(microphone).toBeChecked();
  await usb.check();
  await page.locator('[name="start_debounce_seconds"]').fill("23");
  await usb.focus();
  await page.evaluate(() => {
    (window as any).__audioFixture.mode = "missing";
    document.querySelector<HTMLButtonElement>("#refresh-devices")!.click();
  });
  await expect(page.locator(".device-row").filter({ hasText: "USB interface" })).toContainText("unavailable");
  await expect(usb).toBeChecked();
  await expect(usb).toBeFocused();
  await expect(page.locator('[name="start_debounce_seconds"]')).toHaveValue("23");
  await page.evaluate(() => {
    (window as any).__audioFixture.mode = "failed";
    document.querySelector<HTMLButtonElement>("#refresh-devices")!.click();
  });
  await expect(page.locator("#device-errors")).toContainText("Audio service is unavailable");
  await expect(microphone).toBeChecked();
  await expect(usb).toBeChecked();
  await page.evaluate(() => {
    (window as any).__audioFixture.mode = "connected";
    document.querySelector<HTMLButtonElement>("#refresh-devices")!.click();
  });
  await expect(page.locator("#device-errors")).toHaveText("");
  await page.getByRole("button", { name: "Save audio sources" }).click();
  await expect(page.locator("#toast")).toContainText("Audio sources saved");
  expect(await page.evaluate(() => (window as any).__audioFixture.sourceSaves.at(-1))).toEqual(["native:one:input", "native:two:input"]);
  await expect(page.locator('[name="start_debounce_seconds"]')).toHaveValue("23");
  await page.getByRole("button", { name: "Dashboard", exact: true }).click();
  await expect(page.locator("#audio-summary")).toContainText("Desk microphone, USB interface");
});

test("failed selection save preserves the draft and later retry submits it", async ({ page }) => {
  const usb = page.getByRole("checkbox", { name: /USB interface/ });
  await usb.check();
  await page.evaluate(() => { (window as any).__audioFixture.failSave = true; });
  await page.getByRole("button", { name: "Save audio sources" }).click();
  await expect(page.locator("#toast")).toContainText("Selection could not be saved");
  await expect(usb).toBeChecked();
  await page.evaluate(() => { (window as any).__audioFixture.failSave = false; });
  await page.getByRole("button", { name: "Save audio sources" }).click();
  await expect(page.locator("#toast")).toContainText("Audio sources saved");
  expect(await page.evaluate(() => (window as any).__audioFixture.saved.audio_sources.map((source: any) => source.id))).toEqual(["native:one:input", "native:two:input"]);
});

test("native labels and IDs render as text and unsupported outputs stay disabled", async ({ page }) => {
  await expect(page.locator("#device-list img")).toHaveCount(0);
  await expect(page.getByText("<b>Format unavailable</b>", { exact: true })).toBeVisible();
  const values = await page.locator(".audio-source").evaluateAll((inputs) => inputs.map((input) => (input as HTMLInputElement).value));
  expect(values).toContain('native:id"with"quotes:input');
  await expect(page.getByRole("checkbox", { name: /System speakers/ })).toBeDisabled();
  await expect(page.getByRole("checkbox", { name: /Desk microphone/ })).toBeChecked();
  await expect(page.getByText(/Default format: 2 channels · 96000 Hz/)).toBeVisible();
});

test("unchanged inventory retains controls and compact settings do not overflow", async ({ page }) => {
  await expect(page.locator("#inventory-backend")).toHaveText("Backend: Test backend");
  await page.setViewportSize({ width: 780, height: 760 });
  expect(await page.locator("body").evaluate((body) => body.scrollWidth <= window.innerWidth)).toBeTruthy();
  const original = await page.locator(".audio-source").first().elementHandle();
  const reads = await page.evaluate(() => (window as any).__audioFixture.reads);
  await page.locator("#refresh-devices").click();
  await expect.poll(() => page.evaluate(() => (window as any).__audioFixture.reads)).toBeGreaterThan(reads);
  await expect(page.locator("#refresh-devices")).toBeEnabled();
  expect(await original!.evaluate((input) => input === document.querySelector(".audio-source"))).toBeTruthy();
  await page.screenshot({ path: "test-results/audio-settings-780x760.png", fullPage: true });
});
