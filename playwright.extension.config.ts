// Separate Playwright config for the real-browser extension runtime test
// (tests-extension/runtime.spec.ts). Deliberately not merged into
// playwright.config.ts: that config's `channel` (msedge/chromium per
// platform) and its `webServer` (the app's own vite dev server) are unrelated
// to this test, which drives Chromium directly via launchPersistentContext
// with --load-extension (a Chromium-only flag) against small local fixture
// pages, never the app itself.
import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./tests-extension",
  timeout: 60_000,
  retries: 0,
  reporter: [["list"]],
  workers: 1,
});
