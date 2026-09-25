// Turns a browser-internal tab id into an opaque, per-install, non-reversible
// token before it ever leaves the extension. The real chrome tab id is never
// sent to the native host. `storageArea` and `subtle` are injected so this is
// unit-testable without chrome.* (see test/tab-id.test.mjs).

async function getOrCreateSalt(storageArea) {
  const existing = await storageArea.get("tabIdSalt");
  if (existing.tabIdSalt) return existing.tabIdSalt;
  const bytes = new Uint8Array(16);
  globalThis.crypto.getRandomValues(bytes);
  const salt = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
  await storageArea.set({ tabIdSalt: salt });
  return salt;
}

export async function opaqueTabId(tabId, storageArea, subtle) {
  const salt = await getOrCreateSalt(storageArea);
  const data = new TextEncoder().encode(`${salt}:${tabId}`);
  const digest = await subtle.digest("SHA-256", data);
  return Array.from(new Uint8Array(digest))
    .slice(0, 8)
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}
