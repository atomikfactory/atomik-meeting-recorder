// Real-browser test of the extension runtime: loads the actual (unmodified,
// aside from the test-only patches applied by scripts/build-extension-test.mjs
// -- see that file's header) extension into Chromium via
// launchPersistentContext + --load-extension, serves the Meet fixture pages
// from browser-extension/test/fixtures over a local HTTP server, and reads
// back the messages the extension would have sent to the native-messaging
// host (captured by the test shim into chrome.storage.session) via the
// extension's own service worker.
//
// This complements, but does not replace, the pure-logic node tests
// (browser-extension/test/*.test.mjs) and the Settings UI tests
// (tests/browser-extension.spec.ts): those never load the extension into a
// real browser or exercise content-runtime.js's DOM polling/heartbeat timers
// against a live page.
//
// Requires the Playwright Chromium browser (`npx playwright install
// chromium`) -- extensions need a real Chromium, not the msedge/webkit
// channels used by the rest of this repo's Playwright tests. Skips itself
// with a clear message if Chromium isn't installed or a persistent context
// with an extension can't be launched in this environment.
import { test, expect, chromium, type BrowserContext } from "@playwright/test";
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import os from "node:os";
import { fileURLToPath } from "node:url";
import { buildExtensionForTest } from "../scripts/build-extension-test.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.join(here, "..");
const FIXTURES_DIR = path.join(REPO_ROOT, "browser-extension", "test", "fixtures");

let extensionDir: string;
let server: http.Server;
let baseUrl: string;
let context: BrowserContext | null = null;
let launchUnavailableReason: string | null = null;

function serveFixtures(): Promise<{ server: http.Server; baseUrl: string; port: number }> {
  return new Promise((resolve, reject) => {
    const srv = http.createServer((req, res) => {
      const url = new URL(req.url || "/", "http://127.0.0.1");
      // Only ever serves the fixture files, under the same /meet-fixture/
      // path prefix that's patched into the test extension's manifest.
      const rel = url.pathname.replace(/^\/meet-fixture\//, "");
      const filePath = path.join(FIXTURES_DIR, rel);
      if (!filePath.startsWith(FIXTURES_DIR) || !fs.existsSync(filePath)) {
        res.writeHead(404).end("not found");
        return;
      }
      res.writeHead(200, { "Content-Type": "text/html; charset=utf-8" });
      fs.createReadStream(filePath).pipe(res);
    });
    srv.on("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const address = srv.address();
      if (!address || typeof address === "string") {
        reject(new Error("failed to bind fixture server"));
        return;
      }
      resolve({
        server: srv,
        baseUrl: `http://127.0.0.1:${address.port}/meet-fixture/`,
        port: address.port,
      });
    });
  });
}

test.beforeAll(async () => {
  // The fixture server must be listening before the extension is built: its
  // port is baked into the test manifest's match patterns as a concrete
  // value (Chrome rejects a wildcard port in web_accessible_resources'
  // match patterns -- see build-extension-test.mjs).
  const served = await serveFixtures();
  server = served.server;
  baseUrl = served.baseUrl;
  extensionDir = buildExtensionForTest(undefined, { fixtureServerPort: served.port });

  const userDataDir = fs.mkdtempSync(path.join(os.tmpdir(), "mr-ext-test-"));
  try {
    context = await chromium.launchPersistentContext(userDataDir, {
      // Playwright's own `headless: true` uses Chromium's *old* headless
      // mode, which does not support extensions at all (the service worker
      // never starts -- confirmed while writing this test). Chromium's
      // *new* headless mode does support extensions, but must be requested
      // via the raw `--headless=new` flag with Playwright's `headless` left
      // false, otherwise Playwright's own old-headless flag wins.
      headless: false,
      args: [
        "--headless=new",
        `--disable-extensions-except=${extensionDir}`,
        `--load-extension=${extensionDir}`,
      ],
    });
  } catch (err) {
    launchUnavailableReason =
      `Could not launch Chromium with an unpacked extension (${(err as Error).message}). ` +
      `Run "npx playwright install chromium" and re-run, or check that this environment ` +
      `can launch a (new-headless) Chromium with --load-extension.`;
  }
});

test.afterAll(async () => {
  await context?.close();
  await new Promise<void>((resolve) => server?.close(() => resolve()));
});

async function getServiceWorker() {
  if (!context) throw new Error("no context");
  const existing = context.serviceWorkers();
  if (existing.length) return existing[0];
  return context.waitForEvent("serviceworker", { timeout: 15_000 });
}

async function clearCaptured() {
  const worker = await getServiceWorker();
  await worker.evaluate(() => chrome.storage.session.clear());
}

async function getCaptured(): Promise<any[]> {
  const worker = await getServiceWorker();
  const result = await worker.evaluate(() => chrome.storage.session.get({ capturedMessages: [] }));
  return (result as any).capturedMessages;
}

test.beforeEach(async () => {
  test.skip(!!launchUnavailableReason, launchUnavailableReason ?? "");
  await clearCaptured();
});

test("lobby produces no message at all", async () => {
  const page = await context!.newPage();
  await page.goto(baseUrl + "lobby.html");
  await page.waitForTimeout(1500); // several poll cycles (test build polls every 400ms)
  expect(await getCaptured()).toEqual([]);
  await page.close();
});

test("in-call sends a cleaned-title in_call message with a hashed, non-reversible tab id, then heartbeats", async () => {
  const page = await context!.newPage();
  await page.goto(baseUrl + "in-call.html");

  await expect
    .poll(async () => (await getCaptured()).length, { timeout: 10_000 })
    .toBeGreaterThanOrEqual(1);

  const [first] = await getCaptured();
  expect(first.platform).toBe("meet");
  expect(first.state).toBe("in_call");
  expect(first.title).toBe("Weekly sync");
  expect(first.tab_id).toMatch(/^[0-9a-f]{16}$/);

  // Heartbeat: a second in_call message should follow without any DOM change,
  // on the test build's shortened 2.5s heartbeat.
  await expect
    .poll(async () => (await getCaptured()).length, { timeout: 10_000 })
    .toBeGreaterThanOrEqual(2);
  const messages = await getCaptured();
  expect(messages.every((m) => m.state === "in_call")).toBe(true);

  await page.close();
});

test("clicking the aria-labelled Leave call control ends the call", async () => {
  const page = await context!.newPage();
  await page.goto(baseUrl + "transition.html");

  // Starts in lobby: nothing sent yet.
  await page.waitForTimeout(1000);
  expect(await getCaptured()).toEqual([]);

  await page.getByRole("button", { name: "Join now" }).click();
  await expect
    .poll(async () => (await getCaptured()).length, { timeout: 10_000 })
    .toBeGreaterThanOrEqual(1);
  const afterJoin = await getCaptured();
  expect(afterJoin.at(-1).state).toBe("in_call");
  expect(afterJoin.at(-1).title).toBe("Weekly sync");

  await clearCaptured();
  await page.getByRole("button", { name: "Leave call" }).click();
  await expect
    .poll(async () => {
      const captured = await getCaptured();
      return captured.at(-1)?.state;
    }, { timeout: 10_000 })
    .toBe("ended");
  const afterLeave = await getCaptured();
  expect(afterLeave.at(-1).title).toBeNull();

  await page.close();
});

test("no captured message ever contains the fixture's URL or path", async () => {
  const page = await context!.newPage();
  await page.goto(baseUrl + "in-call.html");
  await expect
    .poll(async () => (await getCaptured()).length, { timeout: 10_000 })
    .toBeGreaterThanOrEqual(1);

  const captured = await getCaptured();
  const serialized = JSON.stringify(captured);
  expect(serialized).not.toContain("127.0.0.1");
  expect(serialized).not.toContain("meet-fixture");
  expect(serialized).not.toContain(baseUrl);

  await page.close();
});
