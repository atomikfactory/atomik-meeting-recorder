import { expect, test } from "@playwright/test";

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const record = { recording_id: "abc", meeting_title: "Team sync", started_at: 1700000000000, ended_at: 1, duration_ms: 1, status: "recorded", tracks: [{ source_id: "mic", name: "Desk mic", direction: "input", sample_rate: 48000, channels: 1, file_path: "", frames_written: 1, dropped_samples: 0, error: null }], error: null, transcription_status: "", appflowy_status: "" };
    const fixture: any = { record, calls: [], searchResults: { meetings: [], took_ms: 1 }, searchError: null };
    Object.assign(window, { __search: fixture, __TAURI_INTERNALS__: { transformCallback: () => 1, invoke: async (command: string, args: any) => {
      fixture.calls.push([command, args]);
      if (command === "plugin:event|listen") return 1;
      if (command === "get_settings") return { auto_record: true, start_debounce_seconds: 10, end_grace_seconds: 30, whisper_model: "base", audio_sources: [] };
      if (command === "get_app_state") return { status: "idle", recording_supported: true, detection_supported: false, appflowy_connected: false, recordings_folder: "", tray_available: true, tray_error: null };
      if (command === "get_recording_state") return { status: "idle", active: null, recent: [fixture.record], error: null };
      if (command === "get_transcription_state") return { inhibited: false, model: { key: "base", available: false }, jobs: [], error: null, backend: "cpu (AVX2)" };
      if (command === "get_detection_state") return { supported: false, enabled: false, phase: "unsupported", detail: "", error: null, seconds_remaining: null, automation_inhibited: false };
      if (command === "get_diarization_state") return { enabled: true, inhibited: false, models: [], jobs: [], error: null };
      if (command === "get_compression_state") return { enabled: true, inhibited: false, jobs: [], error: null };
      if (command === "audio_storage_summary") return { total_bytes: 0, total_original_bytes: 0, recordings: [] };
      if (command === "get_audio_devices") return { backend: "", devices: [], errors: [] };
      if (command === "open_transcript") return null;
      if (command === "search_transcripts") {
        if (fixture.searchError) throw new Error(fixture.searchError);
        return fixture.searchResults;
      }
      throw Error(command);
    } } });
  });
  await page.goto("/");
});

test("typing debounces before searching and renders a highlighted snippet", async ({ page }) => {
  await page.evaluate(() => {
    (window as any).__search.searchResults = {
      meetings: [{
        recording_id: "abc",
        title: "Team sync",
        started_at: 1700000000000,
        hits: [{ kind: "segment", seq: 0, start_ms: 1000, end_ms: 2000, speaker: "You", snippet_html: "We reached a <mark>decision</mark> today" }],
      }],
      took_ms: 2,
    };
  });
  const input = page.locator("#transcript-search-input");
  await input.fill("decision");
  // No query has fired yet; still within the debounce window.
  expect(await page.evaluate(() => (window as any).__search.calls.filter((c: any) => c[0] === "search_transcripts").length)).toBe(0);
  await page.waitForTimeout(300);
  expect(await page.evaluate(() => (window as any).__search.calls.filter((c: any) => c[0] === "search_transcripts").length)).toBe(1);
  await expect(page.locator(".search-meeting")).toContainText("Team sync");
  await expect(page.locator(".search-hit-text mark")).toHaveText("decision");
});

test("Enter searches immediately and Escape clears the query and results", async ({ page }) => {
  await page.evaluate(() => {
    (window as any).__search.searchResults = {
      meetings: [{ recording_id: "abc", title: "Team sync", started_at: 1700000000000, hits: [{ kind: "summary", seq: 0, start_ms: null, end_ms: null, speaker: null, snippet_html: "the plan" }] }],
      took_ms: 1,
    };
  });
  const input = page.locator("#transcript-search-input");
  await input.fill("plan");
  await input.press("Enter");
  await expect(page.locator(".search-meeting")).toBeVisible();
  await input.press("Escape");
  await expect(input).toHaveValue("");
  await expect(page.locator(".search-meeting")).toHaveCount(0);
  await expect(page.locator("#transcript-search-results")).toBeEmpty();
});

test("empty query shows nothing and no-results state renders for a real miss", async ({ page }) => {
  await expect(page.locator("#transcript-search-results")).toBeEmpty();
  await page.evaluate(() => { (window as any).__search.searchResults = { meetings: [], took_ms: 1 }; });
  await page.locator("#transcript-search-input").fill("nothing matches this");
  await page.waitForTimeout(300);
  await expect(page.locator(".search-status")).toContainText("No matches");
});

test("clicking a hit opens that meeting's transcript", async ({ page }) => {
  await page.evaluate(() => {
    (window as any).__search.searchResults = {
      meetings: [{ recording_id: "abc", title: "Team sync", started_at: 1700000000000, hits: [{ kind: "segment", seq: 0, start_ms: 5000, end_ms: 6000, speaker: "You", snippet_html: "ship on Friday" }] }],
      took_ms: 1,
    };
  });
  await page.locator("#transcript-search-input").fill("Friday");
  await page.waitForTimeout(300);
  await page.locator(".search-hit").click();
  expect(await page.evaluate(() => (window as any).__search.calls.find((c: any) => c[0] === "open_transcript"))).toEqual(["open_transcript", { recordingId: "abc" }]);
});

test("a search error is shown without crashing the page", async ({ page }) => {
  await page.evaluate(() => { (window as any).__search.searchError = "search index unavailable"; });
  await page.locator("#transcript-search-input").fill("anything");
  await page.waitForTimeout(300);
  await expect(page.locator(".search-status")).toContainText("search index unavailable");
});
