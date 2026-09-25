// Tauri invoke wrapper and browser-preview detection.
//
// The app runs both inside the Tauri desktop shell and, during development
// and in the Playwright test suite, as a plain page in a browser -- where
// `window.__TAURI_INTERNALS__` is absent and every `invoke` call is mocked
// or must be avoided. `preview` captures that distinction once at module
// load; `invoke` is re-exported unchanged so callers keep their existing
// call sites and generic type parameters.
export { invoke } from "@tauri-apps/api/core";

export const preview = !("__TAURI_INTERNALS__" in window);
