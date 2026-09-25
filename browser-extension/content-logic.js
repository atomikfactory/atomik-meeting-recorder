// Pure, DOM-independent decision logic shared by every platform's content
// script (content-meet.js, content-teams.js, content-zoom.js) and exercised
// directly by node --test in test/content-logic.test.mjs against fixture
// snippets in test/fixtures/. Nothing here touches `document`, `chrome`, or
// the network -- callers extract `{ ariaLabels, titleText, bodyText }` from
// the live page (see extractSignals in each content-*.js) and pass it in.

/** True if any string in `values` matches any pattern in `patterns` (case-insensitive). */
export function matchesAny(values, patterns) {
  return values.some((value) =>
    patterns.some((pattern) => pattern.test(value))
  );
}

/**
 * A platform's in-call/lobby/ended heuristic, expressed as three independent
 * signal checks over the same `{ ariaLabels, titleText, bodyText }` snapshot.
 * Order of evaluation: ended > in_call > lobby > unknown, because an "ended"
 * screen can still contain stale controls from the call that just finished.
 */
export function decideState(signals, config) {
  const ariaLabels = signals.ariaLabels || [];
  const bodyText = signals.bodyText || "";
  const titleText = signals.titleText || "";
  if (
    config.endedPatterns.some((pattern) => pattern.test(bodyText)) ||
    config.endedPatterns.some((pattern) => pattern.test(titleText))
  ) {
    return "ended";
  }
  if (matchesAny(ariaLabels, config.inCallAriaPatterns)) {
    return "in_call";
  }
  if (matchesAny(ariaLabels, config.lobbyAriaPatterns)) {
    return "lobby";
  }
  return "unknown";
}

/**
 * Strips a platform's window/tab-title decoration (e.g. Meet's "Meet - " or
 * a trailing " | Google Meet") down to a bare meeting name, mirroring the
 * title-cleanup approach in src-tauri/src/meeting_title.rs. Returns null for
 * an empty or clearly non-meeting title (lobby/landing pages) so the app
 * falls back to its own heuristics instead of storing a useless string.
 */
export function cleanTitle(rawTitle, config) {
  if (!rawTitle) return null;
  let title = rawTitle.trim();
  for (const prefix of config.titlePrefixes || []) {
    if (title.toLowerCase().startsWith(prefix.toLowerCase())) {
      title = title.slice(prefix.length).trim();
    }
  }
  for (const suffix of config.titleSuffixes || []) {
    if (title.toLowerCase().endsWith(suffix.toLowerCase())) {
      title = title.slice(0, title.length - suffix.length).trim();
    }
  }
  title = title.replace(/^[-|]\s*/, "").replace(/\s*[-|]$/, "").trim();
  if (!title || (config.genericTitles || []).some((g) => g.toLowerCase() === title.toLowerCase())) {
    return null;
  }
  return title.slice(0, 160);
}

export const MEET_CONFIG = {
  platform: "meet",
  // Google Meet's hang-up control uses a stable jsname across recent
  // releases; the aria-label fallback is locale-dependent (English shown
  // here) and is the main documented fragility point -- see
  // docs/browser-extension.md.
  inCallAriaPatterns: [/^leave call$/i, /^leave meeting$/i],
  lobbyAriaPatterns: [/^join now$/i, /^ask to join$/i],
  endedPatterns: [/you('|’)?ve? left the (meeting|call)/i, /^return to home screen/i],
  titlePrefixes: ["Meet - "],
  titleSuffixes: [" - Google Meet"],
  genericTitles: ["Meet", "Google Meet", "Meet - "],
};

export const TEAMS_CONFIG = {
  platform: "teams",
  inCallAriaPatterns: [/^leave$/i, /^leave \(ctrl/i, /hang up/i],
  lobbyAriaPatterns: [/^join now$/i],
  endedPatterns: [/you left the meeting/i, /call ended/i],
  titlePrefixes: [],
  titleSuffixes: [" | Microsoft Teams"],
  genericTitles: ["Microsoft Teams"],
};

export const ZOOM_CONFIG = {
  platform: "zoom",
  inCallAriaPatterns: [/^leave meeting$/i, /^end meeting$/i],
  lobbyAriaPatterns: [/^join$/i, /^join audio$/i],
  endedPatterns: [/meeting has ended/i, /this meeting has been ended/i],
  titlePrefixes: [],
  titleSuffixes: [" - Zoom"],
  genericTitles: ["Zoom Meeting", "Zoom"],
};
