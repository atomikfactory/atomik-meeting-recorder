import { decideState, cleanTitle } from "./content-logic.js";

const HEARTBEAT_MS = 15000;
const POLL_MS = 2000;

/** Wires the shared decision logic to the live DOM and chrome.runtime messaging
 * for one platform config (MEET_CONFIG, TEAMS_CONFIG, or ZOOM_CONFIG). */
export function startWatching(config) {
  let lastState = null;
  let heartbeatTimer = null;

  function extractSignals() {
    const ariaLabels = Array.from(document.querySelectorAll("[aria-label]"))
      .map((el) => el.getAttribute("aria-label") || "")
      .filter(Boolean);
    return {
      ariaLabels,
      titleText: document.title || "",
      bodyText: document.body ? document.body.innerText.slice(0, 4000) : "",
    };
  }

  function send(state, title) {
    try {
      chrome.runtime.sendMessage({
        kind: "meeting-signal",
        platform: config.platform,
        state,
        title: title || null,
      });
    } catch {
      // Extension context can be invalidated mid-navigation; the next poll retries.
    }
  }

  function stopHeartbeat() {
    if (heartbeatTimer) {
      clearInterval(heartbeatTimer);
      heartbeatTimer = null;
    }
  }

  function poll() {
    const signals = extractSignals();
    const decision = decideState(signals, config);
    if (decision === "in_call") {
      const title = cleanTitle(document.title, config);
      if (lastState !== "in_call") {
        send("in_call", title);
        stopHeartbeat();
        heartbeatTimer = setInterval(() => send("in_call", cleanTitle(document.title, config)), HEARTBEAT_MS);
      }
      lastState = "in_call";
    } else if (decision === "ended" || decision === "lobby") {
      if (lastState === "in_call") {
        send("ended", null);
      }
      stopHeartbeat();
      lastState = decision === "lobby" ? null : "ended";
    }
    // "unknown" is not evidence of anything; keep the last known state.
  }

  setInterval(poll, POLL_MS);
  poll();

  window.addEventListener("beforeunload", () => {
    if (lastState === "in_call") send("ended", null);
  });
}
