import test from "node:test";
import assert from "node:assert/strict";
import { decideState, cleanTitle, matchesAny, MEET_CONFIG, TEAMS_CONFIG, ZOOM_CONFIG } from "../content-logic.js";

// Fixture snapshots are hand-derived summaries of the DOM states saved as
// browser-extension/test/fixtures/*.html (lobby, in-call, left-meeting) --
// the fixtures are the source of truth for what a real page looks like; this
// file exercises the pure decision logic that runs against exactly this
// shape of extracted signal.

test("matchesAny is case-insensitive and requires a pattern hit", () => {
  assert.equal(matchesAny(["Leave call"], [/^leave call$/i]), true);
  assert.equal(matchesAny(["Mute microphone"], [/^leave call$/i]), false);
  assert.equal(matchesAny([], [/^leave call$/i]), false);
});

test("Meet lobby: Join now button present, no leave control", () => {
  const signals = {
    ariaLabels: ["Join now", "Turn on microphone", "Turn on camera"],
    titleText: "Meet - abc-defg-hij",
    bodyText: "Ready to join?",
  };
  assert.equal(decideState(signals, MEET_CONFIG), "lobby");
});

test("Meet in-call: Leave call control present", () => {
  const signals = {
    ariaLabels: ["Leave call", "Turn off microphone", "Chat with everyone"],
    titleText: "Meet - Weekly sync",
    bodyText: "Weekly sync",
  };
  assert.equal(decideState(signals, MEET_CONFIG), "in_call");
});

test("Meet ended: 'You left the meeting' screen wins even with stale controls", () => {
  const signals = {
    ariaLabels: ["Leave call"],
    titleText: "Meet - abc-defg-hij",
    bodyText: "You've left the meeting\nRejoin\nReturn to home screen",
  };
  assert.equal(decideState(signals, MEET_CONFIG), "ended");
});

test("Meet unknown: no recognized control at all", () => {
  const signals = { ariaLabels: ["Settings"], titleText: "Meet", bodyText: "" };
  assert.equal(decideState(signals, MEET_CONFIG), "unknown");
});

test("Teams and Zoom in-call detection use their own configs", () => {
  assert.equal(
    decideState({ ariaLabels: ["Hang up"], titleText: "", bodyText: "" }, TEAMS_CONFIG),
    "in_call"
  );
  assert.equal(
    decideState({ ariaLabels: ["Leave Meeting"], titleText: "", bodyText: "" }, ZOOM_CONFIG),
    "in_call"
  );
  assert.equal(
    decideState({ ariaLabels: [], titleText: "", bodyText: "Meeting has ended" }, ZOOM_CONFIG),
    "ended"
  );
});

test("cleanTitle strips Meet's prefix/suffix decoration", () => {
  assert.equal(cleanTitle("Meet - Weekly sync", MEET_CONFIG), "Weekly sync");
  assert.equal(cleanTitle("Design review - Google Meet", MEET_CONFIG), "Design review");
});

test("cleanTitle returns null for empty or generic titles", () => {
  assert.equal(cleanTitle("", MEET_CONFIG), null);
  assert.equal(cleanTitle("Meet - ", MEET_CONFIG), null);
  assert.equal(cleanTitle("Google Meet", MEET_CONFIG), null);
});

test("cleanTitle truncates to 160 characters", () => {
  const long = "Meet - " + "x".repeat(300);
  const cleaned = cleanTitle(long, MEET_CONFIG);
  assert.equal(cleaned.length, 160);
});

test("Teams title strips the ' | Microsoft Teams' suffix", () => {
  assert.equal(cleanTitle("Sprint planning | Microsoft Teams", TEAMS_CONFIG), "Sprint planning");
});
