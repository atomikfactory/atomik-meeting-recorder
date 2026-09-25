import test from "node:test";
import assert from "node:assert/strict";
import { opaqueTabId } from "../tab-id.js";

function fakeStorage() {
  const store = {};
  return {
    async get(key) {
      return { [key]: store[key] };
    },
    async set(values) {
      Object.assign(store, values);
    },
  };
}

test("opaqueTabId never returns the raw tab id", async () => {
  const storage = fakeStorage();
  const id = await opaqueTabId(42, storage, globalThis.crypto.subtle);
  assert.notEqual(id, "42");
  assert.match(id, /^[0-9a-f]{16}$/);
});

test("opaqueTabId is stable for the same tab and salt", async () => {
  const storage = fakeStorage();
  const first = await opaqueTabId(7, storage, globalThis.crypto.subtle);
  const second = await opaqueTabId(7, storage, globalThis.crypto.subtle);
  assert.equal(first, second);
});

test("opaqueTabId differs across tabs and across installs (salts)", async () => {
  const storage = fakeStorage();
  const tabA = await opaqueTabId(1, storage, globalThis.crypto.subtle);
  const tabB = await opaqueTabId(2, storage, globalThis.crypto.subtle);
  assert.notEqual(tabA, tabB);

  const otherInstall = fakeStorage();
  const tabAOtherInstall = await opaqueTabId(1, otherInstall, globalThis.crypto.subtle);
  assert.notEqual(tabA, tabAOtherInstall);
});

test("the salt is generated once and reused", async () => {
  const storage = fakeStorage();
  await opaqueTabId(1, storage, globalThis.crypto.subtle);
  const { tabIdSalt } = await storage.get("tabIdSalt");
  assert.equal(typeof tabIdSalt, "string");
  assert.equal(tabIdSalt.length, 32);
});
