// Entry point injected as a classic (non-module) content script. Content
// scripts cannot use a static top-level `import` statement -- Chrome and
// Firefox both execute a content_scripts `js` file as a classic script
// unless it is loaded some other way; there is no manifest key that turns a
// *content script* into an ES module (the "type": "module" key this file
// used to declare in content_scripts is not part of that schema and is
// silently ignored, so the static `import` below used to throw
// "Cannot use import statement outside a module" in a real browser -- caught
// by tests-extension/runtime.spec.ts, which loads the actual unpacked
// extension into Chromium). Dynamic `import()` *is* available in a classic
// script and does create a real module context for the imported file (so
// content-runtime.js's own static import of content-logic.js still works),
// as long as the imported files are listed in manifest.json's
// web_accessible_resources for the matched hosts.
(async () => {
  const { startWatching } = await import(chrome.runtime.getURL("content-runtime.js"));
  const { MEET_CONFIG } = await import(chrome.runtime.getURL("content-logic.js"));
  startWatching(MEET_CONFIG);
})();
