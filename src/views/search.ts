import { invoke } from "../api";
import { notice } from "../ui/notice";
import { store } from "../store";
import type { SearchHit, SearchResults } from "../types";
import { escapeAttr, escapeHtml } from "../util/escape";

export function searchHitLabel(hit: SearchHit) {
  switch (hit.kind) {
    case "title": return "Meeting title";
    case "summary": return "Summary";
    case "key_point": return "Key point";
    case "decision": return "Decision";
    case "action_item": return "Action item";
    default: return hit.speaker ? escapeHtml(hit.speaker) : "Transcript";
  }
}

export function formatHitTimestamp(ms: number | null) {
  if (ms === null) return "";
  const totalSeconds = Math.max(0, Math.floor(ms / 1000));
  return `${String(Math.floor(totalSeconds / 60)).padStart(2, "0")}:${String(totalSeconds % 60).padStart(2, "0")}`;
}

export function renderTranscriptSearchResults() {
  if (!store.transcriptSearchQuery.trim()) return "";
  if (store.transcriptSearchLoading && !store.transcriptSearchResults) return `<p class="search-status">Searching…</p>`;
  if (store.transcriptSearchError) return `<p class="search-status" role="alert">Could not search: ${escapeHtml(store.transcriptSearchError)}</p>`;
  const meetings = store.transcriptSearchResults?.meetings ?? [];
  if (!meetings.length) return `<p class="search-status">No matches for “${escapeHtml(store.transcriptSearchQuery)}”.</p>`;
  return `<div class="search-results">${meetings.map((meeting) => {
    const started = new Date(meeting.started_at).toLocaleString();
    const hits = meeting.hits.map((hit) => {
      const timestamp = formatHitTimestamp(hit.start_ms);
      return `<li class="search-hit" role="button" tabindex="0" data-open-search-hit="${escapeAttr(meeting.recording_id)}"><span class="search-hit-meta">${escapeHtml(searchHitLabel(hit))}${timestamp ? ` · ${timestamp}` : ""}</span><span class="search-hit-text">${hit.snippet_html}</span></li>`;
    }).join("");
    return `<article class="search-meeting"><header><b>${escapeHtml(meeting.title)}</b><small>${escapeHtml(started)}</small></header><ul>${hits}</ul></article>`;
  }).join("")}</div>`;
}

export function updateTranscriptSearchUI() {
  const container = document.querySelector<HTMLElement>("#transcript-search-results");
  if (container) container.innerHTML = renderTranscriptSearchResults();
  document.querySelectorAll<HTMLElement>("[data-open-search-hit]").forEach((el) => {
    const open = () => invoke("open_transcript", { recordingId: el.dataset.openSearchHit }).catch((error) => notice(String(error), "error"));
    el.onclick = open;
    el.onkeydown = (event) => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); open(); } };
  });
}

export async function runTranscriptSearch() {
  const query = store.transcriptSearchQuery.trim();
  if (!query) {
    store.transcriptSearchResults = null;
    store.transcriptSearchError = null;
    store.transcriptSearchLoading = false;
    updateTranscriptSearchUI();
    return;
  }
  const requestId = ++store.transcriptSearchRequestId;
  store.transcriptSearchLoading = true;
  updateTranscriptSearchUI();
  try {
    const results = await invoke<SearchResults>("search_transcripts", { query, limit: 20 });
    if (requestId !== store.transcriptSearchRequestId) return; // a newer query superseded this one
    store.transcriptSearchResults = results;
    store.transcriptSearchError = null;
  } catch (error) {
    if (requestId !== store.transcriptSearchRequestId) return;
    store.transcriptSearchError = String(error);
  } finally {
    if (requestId === store.transcriptSearchRequestId) {
      store.transcriptSearchLoading = false;
      updateTranscriptSearchUI();
    }
  }
}

export function wireTranscriptSearch() {
  const input = document.querySelector<HTMLInputElement>("#transcript-search-input");
  if (!input || input.dataset.wired) return;
  input.dataset.wired = "true";
  input.oninput = () => {
    store.transcriptSearchQuery = input.value;
    window.clearTimeout(store.transcriptSearchDebounce);
    store.transcriptSearchDebounce = window.setTimeout(runTranscriptSearch, 150);
  };
  input.onkeydown = (event) => {
    if (event.key === "Enter") {
      window.clearTimeout(store.transcriptSearchDebounce);
      runTranscriptSearch();
    } else if (event.key === "Escape") {
      window.clearTimeout(store.transcriptSearchDebounce);
      store.transcriptSearchQuery = "";
      input.value = "";
      store.transcriptSearchResults = null;
      store.transcriptSearchError = null;
      store.transcriptSearchLoading = false;
      updateTranscriptSearchUI();
    }
  };
}
