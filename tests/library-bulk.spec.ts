import { expect, test } from "@playwright/test";

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    (window as any).__mk_r1 = () => ({ recording_id: "r1", started_at: 1700000000000, ended_at: 1700000001000, duration_ms: 1000, status: "recorded", tracks: [{ source_id: "mic", name: "Desk mic", direction: "input", sample_rate: 48000, channels: 1, file_path: "x.wav", frames_written: 480, dropped_samples: 0, error: null }], error: null, meeting_title: "Untranscribed standup", storage_path: "meetings/x--r1", transcription_status: "", appflowy_status: "" });
    (window as any).__mk_r2 = () => ({ recording_id: "r2", started_at: 1700000000000, ended_at: 1700000001000, duration_ms: 1000, status: "recorded", tracks: [{ source_id: "mic", name: "Desk mic", direction: "input", sample_rate: 48000, channels: 1, file_path: "x.wav", frames_written: 480, dropped_samples: 0, error: null }], error: null, meeting_title: "Failed transcript meeting", storage_path: "meetings/x--r2", transcription_status: "", appflowy_status: "" });
    (window as any).__mk_r3 = () => ({ recording_id: "r3", started_at: 1700000000000, ended_at: 1700000001000, duration_ms: 1000, status: "recorded", tracks: [{ source_id: "mic", name: "Desk mic", direction: "input", sample_rate: 48000, channels: 1, file_path: "x.wav", frames_written: 480, dropped_samples: 0, error: null }], error: null, meeting_title: "No summary meeting", storage_path: "meetings/x--r3", transcription_status: "", appflowy_status: "" });
    (window as any).__mk_r4 = () => ({ recording_id: "r4", started_at: 1700000000000, ended_at: 1700000001000, duration_ms: 1000, status: "recorded", tracks: [{ source_id: "mic", name: "Desk mic", direction: "input", sample_rate: 48000, channels: 1, file_path: "x.wav", frames_written: 480, dropped_samples: 0, error: null }], error: null, meeting_title: "Fully synced meeting", storage_path: "meetings/x--r4", transcription_status: "", appflowy_status: "" });
    (window as any).__mk_r5 = () => ({ recording_id: "r5", started_at: 1700000000000, ended_at: 1700000001000, duration_ms: 1000, status: "recorded", tracks: [{ source_id: "mic", name: "Desk mic", direction: "input", sample_rate: 48000, channels: 1, file_path: "x.wav", frames_written: 480, dropped_samples: 0, error: null }], error: null, meeting_title: "Failed summary meeting", storage_path: "meetings/x--r5", transcription_status: "", appflowy_status: "" });
  });
  await page.addInitScript(() => {
    const fixture: any = {
      calls: [] as any[],
      recent: [
        (window as any).__mk_r1(),
        (window as any).__mk_r2(),
        (window as any).__mk_r3(),
        (window as any).__mk_r4(),
        (window as any).__mk_r5(),
      ],
      transcriptionJobs: [
        { recording_id: "r2", status: "failed", progress: 0, error: "boom", markdown_path: null },
        { recording_id: "r3", status: "completed", progress: 100, error: null, markdown_path: "r3.md" },
        { recording_id: "r4", status: "completed", progress: 100, error: null, markdown_path: "r4.md" },
        { recording_id: "r5", status: "completed", progress: 100, error: null, markdown_path: "r5.md" },
      ],
      summaryJobs: [
        { recording_id: "r4", status: "completed", progress: 100, error: null, title: null },
        { recording_id: "r5", status: "failed", progress: 0, error: "no ollama", title: null },
      ],
      appflowyJobs: [
        { recording_id: "r4", status: "completed", error: null, export_index: 1, page_url: "https://x" },
      ],
      deleteFromDiskResult: { results: [] as any[] },
    };
    Object.assign(window, { __bulk: fixture, __TAURI_INTERNALS__: {
      transformCallback: () => 1,
      invoke: async (command: string, args: any) => {
        fixture.calls.push([command, args]);
        if (command === "plugin:event|listen") return 1;
        if (command === "get_settings") return { auto_record: true, start_debounce_seconds: 10, end_grace_seconds: 30, whisper_model: "base", audio_sources: [] };
        if (command === "get_app_state") return { status: "idle", recording_supported: true, detection_supported: false, appflowy_connected: false, recordings_folder: "", tray_available: true, tray_error: null };
        if (command === "get_recording_state") return { status: "idle", active: null, recent: fixture.recent, error: null };
        if (command === "get_transcription_state") return { inhibited: false, model: { key: "base", available: true }, jobs: fixture.transcriptionJobs, error: null, backend: "cpu (AVX2)" };
        if (command === "get_detection_state") return { supported: false, enabled: false, phase: "unsupported", detail: "", error: null, seconds_remaining: null, automation_inhibited: false };
        if (command === "get_diarization_state") return { enabled: true, inhibited: false, models: [], jobs: [], error: null };
        if (command === "get_compression_state") return { enabled: true, inhibited: false, jobs: [], error: null };
        if (command === "get_summary_state") return { enabled: true, inhibited: false, jobs: fixture.summaryJobs, error: null };
        if (command === "get_appflowy_state") return { enabled: true, auto_export: false, connected: true, email: "x@y.com", base_url: "https://beta.appflowy.cloud", jobs: fixture.appflowyJobs, inhibited: false, error: null };
        if (command === "get_browser_extension_status") return { connected: false, last_message_seconds_ago: null, registered: false, browsers: [] };
        if (command === "audio_storage_summary") return { total_bytes: 3_000_000, total_original_bytes: 3_000_000, recordings: fixture.recent.map((r: any) => ({ recording_id: r.recording_id, bytes: 1_000_000, original_bytes: 1_000_000, format: "wav" })) };
        if (command === "get_audio_devices") return { backend: "", devices: [], errors: [] };
        if (command === "transcribe_recording" || command === "retry_transcription" || command === "retry_summary" || command === "queue_summary") return null;
        if (command === "delete_recording") { fixture.recent = fixture.recent.filter((r: any) => r.recording_id !== args.recordingId); return null; }
        if (command === "delete_recordings_from_disk") {
          const results = args.recordingIds.map((id: string) => ({ recording_id: id, deleted: true, skipped_reason: null }));
          fixture.recent = fixture.recent.filter((r: any) => !args.recordingIds.includes(r.recording_id));
          return { results };
        }
        throw Error(`Unexpected ${command}`);
      },
    } });
  });
  await page.goto("/");
});

const calls = (page: any, command: string) => page.evaluate(([command]: any) => (window as any).__bulk.calls.filter((x: any) => x[0] === command), [command]);

test("selection persists across background polls and a list update, and is pruned only for removed rows", async ({ page }) => {
  const r1 = page.locator('[data-select="r1"]');
  await r1.check();
  await expect(r1).toBeChecked();
  // Several poll cycles (get_recording_state polls every ~1s while idle).
  await page.waitForTimeout(3200);
  await expect(r1).toBeChecked();
  await expect(page.locator("#library-selection-bar")).toContainText("1 selected");
  // A new recording appears in the list; selection of the untouched row survives.
  await page.evaluate(() => { (window as any).__bulk.recent = [{ ...(window as any).__mk_r1(), recording_id: "r6", meeting_title: "Brand new meeting" }, ...(window as any).__bulk.recent]; });
  await page.waitForTimeout(1100);
  await expect(page.locator('[data-select="r1"]')).toBeChecked();
  // r2 is removed from the list entirely; its selection is pruned, r1's is not.
  await page.locator('[data-select="r2"]').check();
  await page.evaluate(() => { (window as any).__bulk.recent = (window as any).__bulk.recent.filter((r: any) => r.recording_id !== "r2"); });
  await page.waitForTimeout(1100);
  await expect(page.locator('[data-select="r2"]')).toHaveCount(0);
  await expect(page.locator('[data-select="r1"]')).toBeChecked();
  await expect(page.locator("#library-selection-bar")).toContainText("1 selected");
});

test("select-all (visible) checks every visible row and shows indeterminate for a partial selection", async ({ page }) => {
  const selectAll = page.locator("#library-select-all");
  await page.locator('[data-select="r1"]').check();
  await page.waitForTimeout(1100);
  expect(await selectAll.evaluate((el: HTMLInputElement) => el.indeterminate)).toBe(true);
  await selectAll.check();
  await expect(page.locator('[data-select]:checked')).toHaveCount(5);
  await selectAll.uncheck();
  await expect(page.locator('[data-select]:checked')).toHaveCount(0);
});

test("each filter chip narrows the list and shows a count", async ({ page }) => {
  await expect(page.getByRole("button", { name: /^All \(5\)/ })).toBeVisible();
  await page.getByRole("button", { name: /^Synced to AppFlowy/ }).click();
  await expect(page.locator(".recording-row")).toHaveCount(1);
  await expect(page.locator(".recording-row").first()).toContainText("Fully synced meeting");
  await page.getByRole("button", { name: /^Not synced/ }).click();
  await expect(page.locator(".recording-row")).toHaveCount(4);
  // "No summary" includes every recording without a *completed* summary job
  // (r1, r2, r3 have none at all; r5's failed) -- only r4 (completed) is excluded.
  await page.getByRole("button", { name: /^No summary/ }).click();
  await expect(page.locator(".recording-row")).toHaveCount(4);
  await expect(page.locator(".recording-row", { hasText: "Untranscribed standup" })).toHaveCount(1);
  await expect(page.locator(".recording-row", { hasText: "Fully synced meeting" })).toHaveCount(0);
});

test("bulk queue transcription retries failed jobs and queues untranscribed ones, skipping the rest", async ({ page }) => {
  await page.locator('[data-select="r1"]').check();
  await page.locator('[data-select="r2"]').check();
  await page.locator('[data-select="r4"]').check();
  await page.getByRole("button", { name: "Queue transcription" }).click();
  await expect.poll(() => calls(page, "transcribe_recording")).toEqual([["transcribe_recording", { recordingId: "r1" }]]);
  await expect.poll(() => calls(page, "retry_transcription")).toEqual([["retry_transcription", { recordingId: "r2" }]]);
  await expect(page.locator("#toast")).toContainText("2 queued, 1 skipped");
});

test("bulk queue summary queues a first job for a no-job meeting, retries a failed one, and skips an already-summarized one", async ({ page }) => {
  await page.locator('[data-select="r3"]').check();
  await page.locator('[data-select="r4"]').check();
  await page.locator('[data-select="r5"]').check();
  // Several poll cycles pass before acting, matching how a human would use it.
  await page.waitForTimeout(3200);
  await page.getByRole("button", { name: "Queue summary" }).click();
  // r3 has a completed transcript but no summary job at all yet (e.g. its
  // transcript finished before summaries existed) -- queue_summary enqueues
  // a first job for it. r5's existing job failed -- retry_summary resumes
  // it. r4 already has a completed summary -- skipped.
  await expect.poll(() => calls(page, "queue_summary")).toEqual([["queue_summary", { recordingId: "r3" }]]);
  await expect.poll(() => calls(page, "retry_summary")).toEqual([["retry_summary", { recordingId: "r5" }]]);
  await expect(page.locator("#toast")).toContainText("2 queued, 1 skipped");
});

test("remove from list requires confirmation before calling delete_recording for each selected id", async ({ page }) => {
  await page.locator('[data-select="r1"]').check();
  await page.locator('[data-select="r2"]').check();
  await page.getByRole("button", { name: "Remove from list" }).click();
  await expect(page.locator("#bulk-dialog")).toBeVisible();
  await expect(page.locator("#bulk-dialog-detail")).toContainText("stay on disk");
  await page.locator("#bulk-dialog-cancel").click();
  await expect(calls(page, "delete_recording")).resolves.toEqual([]);
  await page.getByRole("button", { name: "Remove from list" }).click();
  await page.locator("#bulk-dialog-confirm").click();
  await expect.poll(() => calls(page, "delete_recording").then((c: any) => c.length)).toBe(2);
  await expect(page.locator("#toast")).toContainText("2 removed from the list");
});

test("delete from disk defaults to recycle bin (no typing required) and calls the command with mode", async ({ page }) => {
  await page.locator('[data-select="r1"]').check();
  await page.getByRole("button", { name: "Delete from disk" }).click();
  await expect(page.locator("#bulk-dialog-detail")).toContainText("Total audio");
  await expect(page.locator('input[name="bulk-mode"][value="recycle_bin"]')).toBeChecked();
  await expect(page.locator("#bulk-confirm-label")).toBeHidden();
  await expect(page.locator("#bulk-dialog-confirm")).toBeEnabled();
  await page.locator("#bulk-dialog-confirm").click();
  await expect.poll(() => calls(page, "delete_recordings_from_disk")).toEqual([
    ["delete_recordings_from_disk", { recordingIds: ["r1"], mode: "recycle_bin" }],
  ]);
  await expect(page.locator("#toast")).toContainText("Deleted 1, skipped 0");
});

test("permanent delete mode requires typing DELETE or the count before it can be confirmed", async ({ page }) => {
  await page.locator('[data-select="r1"]').check();
  await page.locator('[data-select="r2"]').check();
  await page.getByRole("button", { name: "Delete from disk" }).click();
  await page.locator('input[name="bulk-mode"][value="permanent"]').check();
  await expect(page.locator("#bulk-confirm-label")).toBeVisible();
  await expect(page.locator("#bulk-dialog-confirm")).toBeDisabled();
  await page.locator("#bulk-confirm-input").fill("nope");
  await expect(page.locator("#bulk-dialog-confirm")).toBeDisabled();
  await page.locator("#bulk-confirm-input").fill("2");
  await expect(page.locator("#bulk-dialog-confirm")).toBeEnabled();
  await page.locator("#bulk-confirm-input").fill("DELETE");
  await expect(page.locator("#bulk-dialog-confirm")).toBeEnabled();
  await page.locator("#bulk-dialog-confirm").click();
  await expect.poll(() => calls(page, "delete_recordings_from_disk")).toEqual([
    ["delete_recordings_from_disk", { recordingIds: ["r1", "r2"], mode: "permanent" }],
  ]);
});
