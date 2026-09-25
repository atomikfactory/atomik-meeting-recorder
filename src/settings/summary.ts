import { invoke, preview } from "../api";
import { notice } from "../ui/notice";
import { store } from "../store";
import type { SummaryState } from "../types";
import { escapeAttr, escapeHtml, setTextIfChanged } from "../util/escape";
import { updateRecordingUI } from "../views/library";

export function summaryJob(id: string) {
  return store.summaryState.jobs.find((job) => job.recording_id === id);
}

export function summarySummary(recordingId: string) {
  const job = summaryJob(recordingId);
  if (!job) return store.summaryState.enabled ? "Summary queued" : "Local summary off";
  if (job.status === "summarizing") return `Summarizing ${Math.round(job.progress)}%`;
  if (job.status === "completed") return "Summary ready";
  if (job.status === "waiting_ollama") return "Waiting for Ollama";
  return job.status.replaceAll("_", " ");
}

export async function refreshSummaryState() {
  if (preview || !store.backendReady || document.hidden || store.summaryPoll) return;
  store.summaryPoll = true;
  try {
    store.summaryState = await invoke<SummaryState>("get_summary_state");
  } catch (error) {
    store.summaryState = { ...store.summaryState, error: `Could not check local summaries: ${String(error)}` };
  } finally {
    store.summaryPoll = false;
    updateRecordingUI();
  }
}

export async function refreshSummaryModels(url: string) {
  if (preview || !store.backendReady || store.summaryModelsLoading) return;
  store.summaryModelsLoading = true;
  store.summaryModelsError = null;
  try {
    store.summaryModels = await invoke<string[]>("list_summary_models", { ollamaUrl: url });
  } catch (error) {
    store.summaryModels = [];
    store.summaryModelsError = "Ollama not reachable. Start Ollama and try again.";
  } finally {
    store.summaryModelsLoading = false;
    updateSummarySettingsUI();
  }
}

export function updateSummarySettingsUI() {
  const select = document.querySelector<HTMLSelectElement>('select[name="summary_model"]');
  if (select) {
    const current = select.value || store.settings.summary_model || "qwen3:8b";
    const options = new Set([...store.summaryModels, current]);
    select.innerHTML = [...options].map((name) => `<option value="${escapeAttr(name)}" ${name === current ? "selected" : ""}>${escapeHtml(name)}</option>`).join("");
  }
  setTextIfChanged(document.querySelector("#summary-models-status"), store.summaryModelsError ?? (store.summaryModelsLoading ? "Checking Ollama…" : store.summaryModels.length ? `${store.summaryModels.length} model(s) found on Ollama.` : "Refresh to list models installed in Ollama."));
}

export function wireSummarySection() {
  document.querySelector("#refresh-summary-models")?.addEventListener("click", () => {
    const url = document.querySelector<HTMLInputElement>('input[name="ollama_url"]')?.value ?? store.settings.ollama_url ?? "http://127.0.0.1:11434";
    refreshSummaryModels(url);
  });
  document.querySelector("#test-ollama-connection")?.addEventListener("click", async () => {
    const button = document.querySelector<HTMLButtonElement>("#test-ollama-connection")!;
    const url = document.querySelector<HTMLInputElement>('input[name="ollama_url"]')?.value ?? store.settings.ollama_url ?? "http://127.0.0.1:11434";
    button.disabled = true;
    try {
      await invoke("test_ollama_connection", { ollamaUrl: url });
      notice("Connected to Ollama.");
    } catch (error) {
      notice(`Could not connect to Ollama: ${String(error)}`, "error");
    } finally {
      button.disabled = preview || !store.backendReady;
    }
  });
}
