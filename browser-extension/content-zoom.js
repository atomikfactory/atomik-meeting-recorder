// See content-meet.js for why this uses dynamic import() instead of a
// static top-level import.
(async () => {
  const { startWatching } = await import(chrome.runtime.getURL("content-runtime.js"));
  const { ZOOM_CONFIG } = await import(chrome.runtime.getURL("content-logic.js"));
  startWatching(ZOOM_CONFIG);
})();
