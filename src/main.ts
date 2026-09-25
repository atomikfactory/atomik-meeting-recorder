import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./style.css";
import "./overrides.css";

type Model = "tiny" | "base" | "small" | "medium" | "large" | "turbo";
type Language = "auto" | "en" | "fr" | "es" | "de" | "it" | "pt" | "nl";
type Settings = {
  auto_record: boolean;
  start_debounce_seconds: number;
  end_grace_seconds: number;
  whisper_model: Model;
  audio_sources: AudioSourceSelection[];
  diarization_enabled?: boolean;
  transcription_language?: Language;
  echo_cancellation?: boolean;
  dedupe_echo_segments?: boolean;
  summary_enabled?: boolean;
  ollama_url?: string;
  summary_model?: string;
  compress_audio?: boolean;
};
type AudioSourceSelection = { id: string; name: string; direction: "input" | "output" };
type AudioDevice = { id: string | null; name: string; direction: "input" | "output"; is_default: boolean; default_format: string | null; selectable: boolean; unavailable_reason: string | null };
type AudioInventory = { backend: string; devices: AudioDevice[]; errors: string[] };
type AppState = {
  status: string;
  recording_supported: boolean;
  detection_supported: boolean;
  appflowy_connected: boolean;
  recordings_folder: string;
  tray_available: boolean;
  tray_error: string | null;
};
type Track = {
  source_id: string;
  name: string;
  direction: "input" | "output";
  sample_rate: number;
  channels: number;
  file_path: string;
  frames_written: number;
  dropped_samples: number;
  error: string | null;
};
type Recording = {
  recording_id: string;
  started_at: number;
  ended_at: number | null;
  duration_ms: number;
  status: "recording" | "recorded" | "interrupted" | "failed";
  tracks: Track[];
  error: string | null;
  transcription_status: string;
  auto_transcribe?: boolean;
  appflowy_status: string;
  meeting_title?: string | null;
  storage_path?: string | null;
};
type RecordingState = {
  status: "idle" | "recording" | "processing" | "failed";
  active: Recording | null;
  recent: Recording[];
  error: string | null;
  levels?: {source_id:string;peak_dbfs:number;rms_dbfs:number;signal_detected:boolean;clipping:boolean;last_buffer_age_ms:number|null}[];
};
type TranscriptionJob = { recording_id:string; status:"queued"|"waiting_model"|"transcribing"|"completed"|"failed"; progress:number; error:string|null; markdown_path:string|null; model_key?:string|null };
type TranscriptionState = { inhibited:boolean; model:{key:string;available:boolean;path?:string|null;error?:string|null};jobs:TranscriptionJob[];imported_models?:string[];error:string|null;backend:string };
type DiarizationState = { enabled:boolean; inhibited:boolean; models:{kind:"segmentation"|"embedding";available:boolean;error:string|null}[];jobs:{recording_id:string;status:string;progress:number;error:string|null;speaker_count:number|null}[];error:string|null };
type SummaryJob = { recording_id:string; status:string; progress:number; error:string|null; title:string|null };
type SummaryState = { enabled:boolean; inhibited:boolean; jobs:SummaryJob[]; error:string|null };
type CompressionJob = { recording_id:string; status:string; progress:number; error:string|null; bytes_before:number|null; bytes_after:number|null };
type CompressionState = { enabled:boolean; inhibited:boolean; jobs:CompressionJob[]; error:string|null };
type RecordingAudioStorage = { recording_id:string; bytes:number; original_bytes:number; format:"wav"|"flac"|"mixed"|"none" };
type AudioStorageSummary = { total_bytes:number; total_original_bytes:number; recordings:RecordingAudioStorage[] };
type DetectionState = {
  supported: boolean;
  enabled: boolean;
  phase: string;
  provider: string | null;
  detail: string;
  error: string | null;
  seconds_remaining: number | null;
  automation_inhibited: boolean;
  approval_id?: string | null;
};
const preview = !("__TAURI_INTERNALS__" in window);
let backendReady = !preview;
let settings: Settings = {
  auto_record: true,
  start_debounce_seconds: 10,
  end_grace_seconds: 30,
  whisper_model: "base",
  audio_sources: [],
  transcription_language: "auto",
  echo_cancellation: true,
  dedupe_echo_segments: true,
  summary_enabled: true,
  ollama_url: "http://127.0.0.1:11434",
  summary_model: "qwen3:8b",
  compress_audio: true,
};
let inventory: AudioInventory | null = null;
let selectedSourceIds = new Set<string>();
// Keep a full draft record so unplugged devices retain their last known identity.
let draftSources = new Map<string, AudioSourceSelection>();
let renderedDeviceRows: string | null = null;
let inventoryRefreshing = false;
let audioSaveInFlight = false;
let devicePoll: number | undefined;
let recordingPoll: number | undefined;
let recordingActionInFlight = false;
let recordingPollPromise: Promise<void> | null = null;
let recordingEpoch = 0;
let lastIdleRecordingPoll = 0;
let transcriptionState: TranscriptionState = { inhibited:false, model:{key:"base",available:false}, jobs:[], imported_models:[], error:null, backend:"cpu (AVX2)" };
let diarizationState:DiarizationState={enabled:true,inhibited:false,models:[],jobs:[],error:null}; let diarizationPoll=false;
let summaryState:SummaryState={enabled:true,inhibited:false,jobs:[],error:null}; let summaryPoll=false;
let compressionState:CompressionState={enabled:true,inhibited:false,jobs:[],error:null}; let compressionPoll=false;
let audioStorage:AudioStorageSummary={total_bytes:0,total_original_bytes:0,recordings:[]}; let audioStoragePoll=false;
let summaryModels:string[]=[]; let summaryModelsError:string|null=null; let summaryModelsLoading=false;
const summaryActionInFlight = new Set<string>();
let transcriptionPollInFlight = false;
let renderedRecentRecordings: string | null = null;
let historyQuery = "";
const transcribeInFlight = new Set<string>();
let management: { kind:"rename"|"delete"|"restart"; id:string; title:string } | null = null;
let managementBusy = false;
let recordingState: RecordingState = { status: "idle", active: null, recent: [], error: null };
let detectionState: DetectionState = { supported: false, enabled: false, phase: "unsupported", provider: null, detail: "Meeting detection is available in the desktop app on supported platforms.", error: null, seconds_remaining: null, automation_inhibited: false };
let detectionPollInFlight = false;
let detectionActionInFlight = false;
let state: AppState = {
  status: "idle",
  recording_supported: false,
  detection_supported: false,
  appflowy_connected: false,
  recordings_folder: "Available in the desktop app",
  tray_available: false,
  tray_error: null,
};
const icon = `<svg viewBox="0 0 48 48" aria-hidden="true"><rect x="2" y="2" width="44" height="44" rx="14" fill="#0d746e"/><rect x="19" y="10" width="10" height="19" rx="5" fill="#f7f4ed"/><path d="M14 24a10 10 0 0 0 20 0M24 34v5M18 39h12" fill="none" stroke="#f7f4ed" stroke-width="3" stroke-linecap="round"/></svg>`;

function notice(message: string, kind = "status") {
  const node = document.querySelector<HTMLElement>("#toast");
  if (node) {
    node.textContent = message;
    node.dataset.kind = kind;
  }
}
function escapeHtml(value: string) {
  const node = document.createElement("div");
  node.textContent = value;
  return node.innerHTML;
}
function escapeAttr(value: string) { return escapeHtml(value).replaceAll('"', "&quot;"); }
function setTextIfChanged(node: HTMLElement | null, value: string) {
  if (node && node.textContent !== value) node.textContent = value;
}
function sourceInputId(id: string) {
  return `audio-source-${encodeURIComponent(id)}`;
}
function sourceSummary(sources: AudioSourceSelection[]) {
  if (!sources.length) return "No sources selected";
  const names = sources.map((source) => source.name);
  if (names.length === 1) return names[0];
  if (names.length === 2) return names.join(", ");
  return `${names.slice(0, 2).join(", ")} +${names.length - 2}`;
}
function rememberSource(device: Pick<AudioDevice, "id" | "name" | "direction">) {
  if (device.id) {
    draftSources.set(device.id, {
      id: device.id,
      name: device.name,
      direction: device.direction,
    });
  }
}
function disabled() {
  return preview || !backendReady ? 'disabled aria-disabled="true"' : "";
}
function recordingBusy() {
  return recordingState.status === "recording" || recordingState.status === "processing" || Boolean(recordingState.active);
}
function formatDuration(milliseconds: number) {
  const seconds = Math.max(0, Math.floor(milliseconds / 1000));
  return `${String(Math.floor(seconds / 60)).padStart(2, "0")}:${String(seconds % 60).padStart(2, "0")}`;
}
function recordingDuration(recording: Recording) {
  return formatDuration(recording.status === "recording" ? Date.now() - recording.started_at : recording.duration_ms);
}
function recordingSources(recording: Recording) {
  return recording.tracks.length ? recording.tracks.map((track) => track.name).join(", ") : "No source details available";
}
function levelDb(value:number){return Number.isFinite(value)?Math.max(-96,Math.min(0,value)):-96}
function updateLiveMeters(container: HTMLElement) {
  if (recordingState.status !== "recording" || !recordingState.active) {
    if (container.childElementCount) container.replaceChildren();
    return;
  }
  const tracks = recordingState.active.tracks;
  const sourceIds = new Set(tracks.map((track) => track.source_id));
  container.querySelectorAll<HTMLElement>("[data-level-id]").forEach((row) => {
    if (!sourceIds.has(row.dataset.levelId ?? "")) row.remove();
  });
  const levels = new Map((recordingState.levels ?? []).map((level) => [level.source_id, level]));
  for (const track of tracks) {
    let row = container.querySelector<HTMLElement>(`[data-level-id="${CSS.escape(track.source_id)}"]`);
    if (!row) {
      row = document.createElement("div");
      row.className = "level-row";
      row.dataset.levelId = track.source_id;
      row.innerHTML = `<b></b><small class="level-state"></small><div class="meter" role="meter" aria-valuemin="-96" aria-valuemax="0"><i></i></div><small class="level-db"></small>`;
      container.append(row);
    }
    row.querySelector("b")!.textContent = track.name;
    row.querySelector(".meter")!.setAttribute("aria-label", `${track.name} level`);
    const level = levels.get(track.source_id);
    const valid = !!level && Number.isFinite(level.last_buffer_age_ms) && Number.isFinite(level.peak_dbfs) && Number.isFinite(level.rms_dbfs);
    const stale = valid && level!.last_buffer_age_ms! > 1500;
    const peak = !valid || stale ? -96 : levelDb(level!.peak_dbfs);
    const state = !valid ? "Waiting for audio" : stale ? "No recent audio" : level!.clipping ? "Clipping" : level!.signal_detected ? "Signal detected" : "Quiet";
    row.classList.toggle("is-clipping", valid && !!level?.clipping && !stale);
    row.querySelector(".level-state")!.textContent = `${track.direction === "output" ? "System audio / output" : "Mic / input"} · ${state}`;
    row.querySelector(".meter")!.setAttribute("aria-valuenow", String(peak));
    (row.querySelector(".meter i") as HTMLElement).style.transform = `scaleX(${(peak + 96) / 96})`;
    row.querySelector(".level-db")!.textContent = `${peak.toFixed(0)} dBFS`;
  }
}
function recordingLabel(status: RecordingState["status"]) {
  return status === "recording" ? "Recording" : status === "processing" ? "Saving recording" : status === "failed" ? "Recording failed" : "Ready to record";
}
function detectionLabel() {
  const labels: Record<string, string> = {
    disabled: "Automatic recording off", unsupported: "Automatic detection unavailable",
    waiting: "Waiting for a meeting", confirming: "Confirming meeting activity",
    awaiting_approval: "Record this meeting?",
    approval_received: "Preparing approved recording",
    recording: "Meeting recording active", grace: "Waiting for the meeting to end",
    suppressed: "Automatic recording paused", error: "Detection needs attention",
  };
  return labels[detectionState.phase] ?? "Checking meeting activity";
}
function updateDetectionUI() {
  setTextIfChanged(document.querySelector("#detection-status"), detectionLabel());
  setTextIfChanged(document.querySelector("#detection-detail"), detectionState.detail);
  setTextIfChanged(document.querySelector("#detection-error"), detectionState.error ?? "");
  const seconds = detectionState.seconds_remaining;
  setTextIfChanged(document.querySelector("#detection-countdown"), seconds === null ? "" : `${seconds}s`);
  const panel = document.querySelector<HTMLElement>("#detection-panel");
  if (panel) panel.dataset.phase = detectionState.phase;
  const prompt = document.querySelector<HTMLElement>("#detection-prompt");
  if (prompt) prompt.hidden = detectionState.phase !== "awaiting_approval";
  document.querySelectorAll<HTMLButtonElement>("#detection-prompt button").forEach((button) => {
    button.disabled = preview || !backendReady || detectionActionInFlight || !detectionState.approval_id;
  });
  const help = detectionState.automation_inhibited
    ? "Automatic capture is disabled for this development test session. Your saved preference is unchanged."
    : detectionState.supported
      ? "Detects likely meetings locally and asks before each recording. Your end grace applies after approval. Manual controls are always available."
      : "Automatic detection is not available here. You can still start and stop recordings manually.";
  setTextIfChanged(document.querySelector("#detection-settings-help"), help);
}
async function refreshDetectionState() {
  if (preview || !backendReady || document.hidden || detectionPollInFlight) return;
  detectionPollInFlight = true;
  try {
    const next = await invoke<DetectionState>("get_detection_state");
    const newRequest = next.phase === "awaiting_approval" && next.approval_id !== detectionState.approval_id;
    detectionState = next;
    if (newRequest) {
      document.querySelector<HTMLButtonElement>("[data-page='dashboard']")?.click();
      document.getElementById("detection-panel")?.scrollIntoView({ block: "center", behavior: "instant" });
    }
  } catch (error) {
    detectionState = { ...detectionState, phase: "error", seconds_remaining: null, error: `Could not check meeting detection: ${String(error)}` };
  } finally {
    detectionPollInFlight = false;
    updateDetectionUI();
  }
}
function recordingIssues(recording: Recording) {
  if (recording.error) return recording.error;
  const trackErrors = recording.tracks.flatMap((track) => track.error ? [`${track.name}: ${track.error}`] : []);
  const dropped = recording.tracks.filter((track) => track.dropped_samples > 0).map((track) => `${track.name}: ${track.dropped_samples} samples dropped`);
  return [...trackErrors, ...dropped].join(" · ");
}
function transcriptionJob(id:string) { return transcriptionState.jobs.find((job)=>job.recording_id===id); }
function transcriptionSummary(recording: Recording) {
  const job = transcriptionJob(recording.recording_id);
  if (job?.status === "transcribing") return `Transcribing ${Math.round(job.progress)}%`;
  if (job) return job.status.replaceAll("_", " ");
  if (recording.status === "recording") return "Transcription follows capture";
  if (recording.status === "failed") return "Capture failed — not queued";
  if (recording.auto_transcribe) return transcriptionState.inhibited ? "Processing disabled for this test session" : "Waiting for processing queue";
  return "Not queued — legacy recordings are never transcribed automatically.";
}
function audioSummary(recording: Recording) { return recording.status === "recorded" ? "Audio saved" : recording.status === "interrupted" ? "Audio saved with interruptions" : "Audio failed"; }
function formatBytes(bytes: number) {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 MB";
  const mb = bytes / (1024 * 1024);
  return mb >= 1024 ? `${(mb / 1024).toFixed(2)} GB` : `${mb.toFixed(1)} MB`;
}
function audioStorageEntry(recordingId: string) { return audioStorage.recordings.find((r) => r.recording_id === recordingId); }
function audioStorageLine(recordingId: string) {
  const entry = audioStorageEntry(recordingId);
  if (!entry || entry.format === "none" || entry.bytes <= 0) return "";
  const saved = entry.original_bytes > entry.bytes && entry.original_bytes > 0 ? Math.round((1 - entry.bytes / entry.original_bytes) * 100) : 0;
  const label = entry.format === "flac" ? "FLAC" : entry.format === "wav" ? "WAV" : "Mixed";
  return saved > 0 ? `${label} · ${formatBytes(entry.bytes)} (saved ${saved}%)` : `${label} · ${formatBytes(entry.bytes)}`;
}
function compressionSummary(recordingId: string) {
  const job = compressionState.jobs.find((item) => item.recording_id === recordingId);
  if (job?.status === "failed") return "Audio compression failed";
  if (job?.status === "compressing") return "Compressing audio";
  return "";
}
function audioStorageSummaryText() {
  if (!audioStorage.total_bytes) return "Audio on disk: not yet measured.";
  const saved = audioStorage.total_original_bytes > audioStorage.total_bytes && audioStorage.total_original_bytes > 0
    ? Math.round((1 - audioStorage.total_bytes / audioStorage.total_original_bytes) * 100)
    : 0;
  return saved > 0
    ? `Audio on disk: ${formatBytes(audioStorage.total_bytes)} (was ${formatBytes(audioStorage.total_original_bytes)}, saved ${saved}%).`
    : `Audio on disk: ${formatBytes(audioStorage.total_bytes)}.`;
}
function transcriptSummary(recording: Recording) { const job=transcriptionJob(recording.recording_id); if(!job)return "Transcript not started"; return job.status === "waiting_model" ? `Transcript needs model${job.model_key?`: ${job.model_key}`:""}` : job.status === "queued" ? "Transcript queued" : job.status === "transcribing" ? `Transcript working ${Math.round(job.progress)}%` : job.status === "completed" ? "Transcript completed" : "Transcript failed"; }
function diarizationSummary(recordingId: string) {
  const job = diarizationState.jobs.find((item) => item.recording_id === recordingId);
  if (!job) return diarizationState.enabled ? "Waiting for speaker identification" : "Speaker identification off";
  if (job.status === "completed") return job.speaker_count === null ? "Speaker labels completed" : `${job.speaker_count} speakers estimated`;
  if (job.status === "diarizing") return `Identifying speakers ${Math.round(job.progress)}%`;
  return job.status.replaceAll("_", " ");
}
async function refreshTranscriptionState() { if(preview||!backendReady||document.hidden||transcriptionPollInFlight)return; transcriptionPollInFlight=true; try { transcriptionState=await invoke<TranscriptionState>("get_transcription_state"); } catch (error) { transcriptionState={...transcriptionState,error:`Could not check local transcription: ${String(error)}`}; } finally {transcriptionPollInFlight=false; updateTranscriptionUI();} }
async function refreshDiarizationState(){if(preview||!backendReady||document.hidden||diarizationPoll)return;diarizationPoll=true;try{diarizationState=await invoke<DiarizationState>("get_diarization_state")}catch(error){diarizationState={...diarizationState,error:`Could not check speaker identification: ${String(error)}`}}finally{diarizationPoll=false;updateDiarizationUI()}}
function summaryJob(id:string) { return summaryState.jobs.find((job)=>job.recording_id===id); }
function summarySummary(recordingId: string) {
  const job = summaryJob(recordingId);
  if (!job) return summaryState.enabled ? "Summary queued" : "Local summary off";
  if (job.status === "summarizing") return `Summarizing ${Math.round(job.progress)}%`;
  if (job.status === "completed") return "Summary ready";
  if (job.status === "waiting_ollama") return "Waiting for Ollama";
  return job.status.replaceAll("_", " ");
}
async function refreshSummaryState(){if(preview||!backendReady||document.hidden||summaryPoll)return;summaryPoll=true;try{summaryState=await invoke<SummaryState>("get_summary_state")}catch(error){summaryState={...summaryState,error:`Could not check local summaries: ${String(error)}`}}finally{summaryPoll=false;updateRecordingUI()}}
async function refreshCompressionState(){if(preview||!backendReady||document.hidden||compressionPoll)return;compressionPoll=true;try{compressionState=await invoke<CompressionState>("get_compression_state")}catch(error){compressionState={...compressionState,error:`Could not check audio compression: ${String(error)}`}}finally{compressionPoll=false;updateRecordingUI()}}
async function refreshAudioStorage(){if(preview||!backendReady||document.hidden||audioStoragePoll)return;audioStoragePoll=true;try{audioStorage=await invoke<AudioStorageSummary>("audio_storage_summary")}catch{/* best-effort only */}finally{audioStoragePoll=false;updateRecordingUI()}}
async function refreshSummaryModels(url: string) {
  if (preview || !backendReady || summaryModelsLoading) return;
  summaryModelsLoading = true;
  summaryModelsError = null;
  try {
    summaryModels = await invoke<string[]>("list_summary_models", { ollamaUrl: url });
  } catch (error) {
    summaryModels = [];
    summaryModelsError = "Ollama not reachable. Start Ollama and try again.";
  } finally {
    summaryModelsLoading = false;
    updateSummarySettingsUI();
  }
}
function updateSummarySettingsUI() {
  const select = document.querySelector<HTMLSelectElement>('select[name="summary_model"]');
  if (select) {
    const current = select.value || settings.summary_model || "qwen3:8b";
    const options = new Set([...summaryModels, current]);
    select.innerHTML = [...options].map((name) => `<option value="${escapeAttr(name)}" ${name === current ? "selected" : ""}>${escapeHtml(name)}</option>`).join("");
  }
  setTextIfChanged(document.querySelector("#summary-models-status"), summaryModelsError ?? (summaryModelsLoading ? "Checking Ollama…" : summaryModels.length ? `${summaryModels.length} model(s) found on Ollama.` : "Refresh to list models installed in Ollama."));
}
function updateDiarizationUI(){const node=document.querySelector<HTMLElement>("#diarization-status");const segmentation=diarizationState.models.find(m=>m.kind==="segmentation");const embedding=diarizationState.models.find(m=>m.kind==="embedding");const modelError=[segmentation,embedding].flatMap(m=>m?.error?[m.error]:[]).join(" ");if(node)node.textContent=diarizationState.error??(diarizationState.inhibited?"Speaker identification is temporarily inhibited.":modelError||(segmentation?.available&&embedding?.available?"Local speaker identification ready.":"Choose both local models to identify speakers. Transcription remains available."));updateRecordingUI()}
function updateTranscriptionUI() {
  setTextIfChanged(document.querySelector("#model-status"), transcriptionState.model.available
    ? `Ready: ${transcriptionState.model.key}` : (transcriptionState.model.error ?? `Import the ${transcriptionState.model.key || "selected"} model to begin`));
  setTextIfChanged(document.querySelector("#backend-status"), `Backend: ${transcriptionState.backend}`);
  const imported = transcriptionState.imported_models ?? [];
  document.querySelectorAll<HTMLOptionElement>('select[name="whisper_model"] option').forEach((option) => {
    const size = option.value as Model;
    option.textContent = `${size}${imported.includes(size) ? "" : " (not imported)"}`;
  });
  setTextIfChanged(document.querySelector("#model-select-warning"), imported.includes(settings.whisper_model)
    ? "" : `${settings.whisper_model} is not imported yet; jobs will wait until you import it or pick an imported size.`);
  const jobs = document.querySelector<HTMLElement>("#transcription-jobs");
  const content = `${transcriptionState.inhibited ? "<p>Local transcription is temporarily inhibited.</p>" : ""}${transcriptionState.error ? `<p class="recording-error" role="alert">${escapeHtml(transcriptionState.error)}</p>` : ""}` +
    (transcriptionState.jobs.slice(0, 25).map(job => `<div><b>${escapeHtml(job.status.replaceAll("_", " "))}</b> <small>Recording ${escapeHtml(job.recording_id.slice(0, 8))}</small>${job.status === "transcribing" ? ` <span>${Math.round(job.progress)}%</span>` : ""}${job.error ? `<small>${escapeHtml(job.error)}</small>` : ""}</div>`).join("") || "<small>No recordings are queued.</small>");
  if (jobs && jobs.innerHTML !== content) jobs.innerHTML = content;
  updateRecordingUI();
}
async function loadDesktop() {
  try {
    const [saved, app] = await Promise.all([
      invoke<Settings>("get_settings"),
      invoke<AppState>("get_app_state"),
    ]);
    settings = { ...saved, audio_sources: saved.audio_sources ?? [] };
    selectedSourceIds = new Set(settings.audio_sources.map((source) => source.id));
    draftSources = new Map(settings.audio_sources.map((source) => [source.id, source]));
    state = app;
    backendReady = true;
    render();
    refreshDevices();
    refreshRecordingState();
    refreshDetectionState();
    refreshTranscriptionState();
    refreshDiarizationState();
    refreshSummaryState();
    refreshCompressionState();
    refreshAudioStorage();
    refreshSummaryModels(settings.ollama_url ?? "http://127.0.0.1:11434");
  } catch (error) {
    backendReady = false;
    render();
    notice(`Desktop connection failed: ${String(error)}`, "error");
  }
}
function deviceRows() {
  const known = new Map((inventory?.devices ?? []).filter((device) => device.id).map((device) => [device.id!, device]));
  const unavailable = [...selectedSourceIds]
    .filter((id) => !known.has(id))
    .map((id) => draftSources.get(id) ?? { id, name: "Previously selected audio device", direction: "input" as const });
  const rows = [...(inventory?.devices ?? [])];
  for (const source of unavailable) rows.push({ id: source.id, name: source.name, direction: source.direction, is_default: false, default_format: null, selectable: true, unavailable_reason: "Currently unavailable. Kept selected until you remove it." });
  if (!rows.length) return `<p class="device-empty">No audio endpoints were reported. Your saved choices will remain unchanged.</p>`;
  return rows.map((device) => {
    const id = device.id;
    const checked = id && selectedSourceIds.has(id) ? "checked" : "";
    const canChoose = Boolean(id && device.selectable) || Boolean(id && checked);
    const note = device.unavailable_reason ?? (device.default_format ? `Default format: ${device.default_format}` : "Default format unavailable");
    const inputId = sourceInputId(id ?? "");
    return `<label class="device-row ${!canChoose ? "device-disabled" : ""}" for="${escapeAttr(inputId)}"><input id="${escapeAttr(inputId)}" type="checkbox" class="audio-source" value="${escapeAttr(id ?? "")}" ${checked} ${canChoose && !preview && backendReady && !audioSaveInFlight && !recordingBusy() ? "" : "disabled"}/><span><b>${escapeHtml(device.name)}</b><small>${device.direction === "input" ? "Microphone / input" : "System output"}${device.is_default ? " (system default)" : ""}</small><small>${escapeHtml(note)}</small></span></label>`;
  }).join("");
}
function refreshDeviceSection() {
  const list = document.querySelector<HTMLElement>("#device-list");
  const focusedId = document.activeElement instanceof HTMLElement ? document.activeElement.id : "";
  const nextRows = deviceRows();
  if (list && renderedDeviceRows !== nextRows) {
    list.innerHTML = nextRows;
    renderedDeviceRows = nextRows;
    if (focusedId) document.getElementById(focusedId)?.focus();
  }
  const errors = document.querySelector<HTMLElement>("#device-errors");
  setTextIfChanged(errors, inventory?.errors.join(" ") ?? "");
  const backend = document.querySelector<HTMLElement>("#inventory-backend");
  setTextIfChanged(
    backend,
    inventory?.backend ? `Backend: ${inventory.backend}` : "Backend unavailable",
  );
  document.querySelectorAll<HTMLInputElement>(".audio-source").forEach((input) => input.onchange = () => {
    if (input.checked) {
      selectedSourceIds.add(input.value);
      const device = inventory?.devices.find((item) => item.id === input.value);
      if (device) rememberSource(device);
    } else selectedSourceIds.delete(input.value);
  });
}
async function refreshDevices() {
  if (preview || !backendReady || inventoryRefreshing || document.hidden) return;
  inventoryRefreshing = true;
  const refresh = document.querySelector<HTMLButtonElement>("#refresh-devices");
  if (refresh) refresh.disabled = true;
  try {
    inventory = await invoke<AudioInventory>("get_audio_devices");
    for (const device of inventory.devices) {
      if (device.id && selectedSourceIds.has(device.id)) rememberSource(device);
    }
    refreshDeviceSection();
  }
  catch (error) { const errors = document.querySelector<HTMLElement>("#device-errors"); if (errors) errors.textContent = `Could not refresh audio devices: ${String(error)}`; }
  finally { inventoryRefreshing = false; if (refresh) refresh.disabled = false; }
}
function recentRecordings() {
  if (!recordingState.recent.length) return `<div class="empty"><div class="empty-wave">~~~</div><h3>Your recordings will appear here</h3><p>Audio and transcripts stay on this device. Import a local Whisper model in Settings to begin transcription. AppFlowy export is coming later.</p></div>`;
  const query=historyQuery.toLocaleLowerCase(); const visible=recordingState.recent.filter(r=>!query||`${r.meeting_title??"Meeting"} ${new Date(r.started_at).toLocaleString()}`.toLocaleLowerCase().includes(query));
  return `<p class="history-scope">Searching the 25 most recent recordings. Older recordings stay in their original folders; use Open folder on a recording.</p><div class="recording-list">${visible.map((recording) => {
    const issues = recordingIssues(recording);
    const shortId = recording.recording_id.slice(0, 8);
    const started = new Date(recording.started_at).toLocaleString();
    const job=transcriptionJob(recording.recording_id); const diarization=diarizationState.jobs.find((item)=>item.recording_id===recording.recording_id); const summary=summaryJob(recording.recording_id); const folder=`<button class="secondary" data-open-folder="${escapeAttr(recording.recording_id)}">Open folder</button>`; const action=job?.status==="completed"?`<button class="secondary" data-open-transcript="${escapeAttr(recording.recording_id)}">Open transcript</button>`:job?.status==="failed"?`<button class="secondary" data-retry-transcript="${escapeAttr(recording.recording_id)}">Retry transcription</button>`:job?.status==="waiting_model"?`<button class="secondary" data-choose-model>Choose Whisper model${job.model_key?`: ${escapeHtml(job.model_key)}`:""}</button>`:!job&&["recorded","interrupted","failed"].includes(recording.status)&&recording.tracks.some(t=>t.frames_written>0)?`<button class="secondary" data-transcribe="${escapeAttr(recording.recording_id)}" ${transcribeInFlight.has(recording.recording_id)?"disabled":""}>Transcribe</button>`:""; const retry=diarization?.status==="failed"?`<button class="secondary" data-retry-diarization="${escapeAttr(recording.recording_id)}">Retry speaker identification</button>`:"";
    const summaryBusy=summaryActionInFlight.has(recording.recording_id);
    const summaryActions=summary?.status==="completed"?`<button class="secondary" data-open-summary="${escapeAttr(recording.recording_id)}" ${summaryBusy?"disabled":""}>Open summary</button><button class="secondary" data-regenerate-summary="${escapeAttr(recording.recording_id)}" ${summaryBusy?"disabled":""}>Regenerate summary</button>${summary.title&&summary.title!==(recording.meeting_title??"Meeting")?`<button class="secondary" data-use-suggested-title="${escapeAttr(recording.recording_id)}" data-title="${escapeAttr(summary.title)}" ${summaryBusy?"disabled":""}>Use suggested title: ${escapeHtml(summary.title)}</button>`:""}`:summary?.status==="failed"?`<button class="secondary" data-retry-summary="${escapeAttr(recording.recording_id)}" ${summaryBusy?"disabled":""}>Retry summary</button>`:"";
    const manage=`<button class="secondary" data-rename="${escapeAttr(recording.recording_id)}">Rename</button><button class="secondary" data-delete="${escapeAttr(recording.recording_id)}">Remove</button>${["completed","failed","waiting_model"].includes(job?.status??"")?`<button class="secondary" data-restart="${escapeAttr(recording.recording_id)}">Restart transcription</button>`:""}`;
    const storageLine = audioStorageLine(recording.recording_id); const compressionLine = compressionSummary(recording.recording_id);
    return `<article class="recording-row"><div><b>${escapeHtml(recording.meeting_title||"Meeting")}</b><small>${escapeHtml(started)} · ${recordingDuration(recording)}</small><small>${escapeHtml(recordingSources(recording))}</small><small>${escapeHtml(audioSummary(recording))}${storageLine ? ` · ${escapeHtml(storageLine)}` : ""}${compressionLine ? ` · ${escapeHtml(compressionLine)}` : ""} · ${escapeHtml(transcriptSummary(recording))} · ${escapeHtml(diarizationSummary(recording.recording_id))} · ${escapeHtml(summarySummary(recording.recording_id))}</small><small>Recording ${escapeHtml(shortId)}</small></div>${folder}${action}${retry}${summaryActions}${manage}${issues ? `<p>${escapeHtml(issues)}</p>` : ""}${diarization?.error ? `<p>${escapeHtml(diarization.error)}</p>` : ""}${summary?.error ? `<p>${escapeHtml(summary.error)}</p>` : ""}</article>`;
  }).join("")}</div>`;
}
function updateRecordingUI() {
  const active = recordingState.active;
  const live = recordingState.status === "recording" && Boolean(active);
  const status = document.querySelector<HTMLElement>("#recording-status");
  setTextIfChanged(status, recordingLabel(recordingState.status));
  if (status) status.dataset.active = live ? "true" : "false";
  const panel = document.querySelector<HTMLElement>(".recording-panel");
  if (panel) {
    panel.dataset.active = live ? "true" : "false";
    panel.classList.toggle("is-active", live);
  }
  document.body.classList.toggle("is-recording", recordingState.status === "recording" && Boolean(active));
  const topStatus = document.querySelector<HTMLElement>("#top-recording-status");
  setTextIfChanged(topStatus, recordingLabel(recordingState.status));
  setTextIfChanged(document.querySelector("#recording-heading"), recordingState.status === "recording" ? "Recording" : recordingState.status === "processing" ? "Saving recording" : "Ready to record");
  const timer = document.querySelector<HTMLElement>("#recording-timer");
  setTextIfChanged(timer, active ? recordingDuration(active) : "00:00");
  const sources = document.querySelector<HTMLElement>("#active-sources");
  setTextIfChanged(sources, active ? recordingSources(active) : sourceSummary(settings.audio_sources));
  const meters=document.querySelector<HTMLElement>("#live-meters"); if(meters) updateLiveMeters(meters);
  const error = document.querySelector<HTMLElement>("#recording-error");
  const warning=recordingState.status==="recording"&&(recordingState.error??active?.error); setTextIfChanged(error, warning?`Recording continues; audio may contain gaps. ${warning}`:(recordingState.error??active?.error??"")); if(error) error.dataset.warning=warning?"true":"false";
  const record = document.querySelector<HTMLButtonElement>("#record");
  if (record) {
    record.disabled = preview || !backendReady || !state.recording_supported || recordingActionInFlight || recordingBusy();
    const stop = document.querySelector<HTMLButtonElement>("#stop-recording");
    if (stop) {
      stop.hidden = !active;
      stop.disabled = preview || !backendReady || recordingActionInFlight || !active;
    }
  }
  const nextNote = document.querySelector<HTMLElement>("#capture-next");
  setTextIfChanged(nextNote, recordingBusy() ? "Recording is active. Source changes are saved for the next recording." : "Select one or more sources for your next local recording.");
  const saveSources = document.querySelector<HTMLButtonElement>("#save-audio-sources");
  if (saveSources) saveSources.disabled = preview || !backendReady || audioSaveInFlight || recordingBusy();
  refreshDeviceSection();
  const meetings = document.querySelector<HTMLElement>("#recent-recordings");
  const nextRecent = recentRecordings();
  if (meetings && renderedRecentRecordings !== nextRecent) {
    const focused = document.activeElement as HTMLElement | null;
    const action = focused?.dataset.openTranscript ? "data-open-transcript" : focused?.dataset.retryTranscript ? "data-retry-transcript" : focused?.dataset.openFolder ? "data-open-folder" : focused?.dataset.transcribe ? "data-transcribe" : null;
    const focusedRecording = focused?.dataset.openTranscript ?? focused?.dataset.retryTranscript;
    meetings.innerHTML = nextRecent;
    renderedRecentRecordings = nextRecent;
    if (action && focusedRecording) Array.from(meetings.querySelectorAll<HTMLButtonElement>(`[${action}]`)).find(button => button.getAttribute(action) === focusedRecording)?.focus();
  }
  document.querySelectorAll<HTMLButtonElement>("[data-open-transcript]").forEach(button=>button.onclick=()=>invoke("open_transcript",{recordingId:button.dataset.openTranscript}).catch(error=>notice(String(error),"error")));
  document.querySelectorAll<HTMLButtonElement>("[data-open-folder]").forEach(button=>button.onclick=()=>invoke("open_recording_folder",{recordingId:button.dataset.openFolder}).catch(error=>notice(String(error),"error")));
  document.querySelectorAll<HTMLButtonElement>("[data-transcribe]").forEach(button=>button.onclick=async()=>{const id=button.dataset.transcribe!;if(transcribeInFlight.has(id))return;transcribeInFlight.add(id);updateRecordingUI();try{await invoke("transcribe_recording",{recordingId:id});await refreshTranscriptionState()}catch(error){notice(`Could not queue transcription: ${String(error)}`,"error")}finally{transcribeInFlight.delete(id);updateRecordingUI()}});
  document.querySelectorAll<HTMLButtonElement>("[data-choose-model]").forEach(button=>button.onclick=()=>{document.querySelector<HTMLButtonElement>("[data-page='settings']")?.click();setTimeout(()=>document.querySelector<HTMLButtonElement>("#choose-model")?.focus(),0)});
  const search=document.querySelector<HTMLInputElement>("#history-search"); if(search) search.oninput=event=>{historyQuery=(event.target as HTMLInputElement).value;renderedRecentRecordings=null;updateRecordingUI();};
  document.querySelectorAll<HTMLButtonElement>("[data-retry-transcript]").forEach(button=>button.onclick=async()=>{try{await invoke("retry_transcription",{recordingId:button.dataset.retryTranscript});await refreshTranscriptionState();}catch(error){notice(`Could not retry transcription: ${String(error)}`,"error");}});
  document.querySelectorAll<HTMLButtonElement>("[data-retry-diarization]").forEach(button=>button.onclick=async()=>{try{await invoke("retry_diarization",{recordingId:button.dataset.retryDiarization});await refreshDiarizationState();}catch(error){notice(`Could not retry speaker identification: ${String(error)}`,"error");}});
  document.querySelectorAll<HTMLButtonElement>("[data-open-summary]").forEach(button=>button.onclick=()=>invoke("open_summary",{recordingId:button.dataset.openSummary}).catch(error=>notice(String(error),"error")));
  const runSummaryAction=async(id:string,command:string)=>{if(summaryActionInFlight.has(id))return;summaryActionInFlight.add(id);updateRecordingUI();try{await invoke(command,{recordingId:id});await refreshSummaryState();}catch(error){notice(`Could not update summary: ${String(error)}`,"error");}finally{summaryActionInFlight.delete(id);updateRecordingUI();}};
  document.querySelectorAll<HTMLButtonElement>("[data-retry-summary]").forEach(button=>button.onclick=()=>runSummaryAction(button.dataset.retrySummary!,"retry_summary"));
  document.querySelectorAll<HTMLButtonElement>("[data-regenerate-summary]").forEach(button=>button.onclick=()=>runSummaryAction(button.dataset.regenerateSummary!,"regenerate_summary"));
  document.querySelectorAll<HTMLButtonElement>("[data-use-suggested-title]").forEach(button=>button.onclick=async()=>{const id=button.dataset.useSuggestedTitle!;const title=button.dataset.title!;try{await invoke("rename_recording",{recordingId:id,title});await refreshRecordingState(true);}catch(error){notice(`Could not use the suggested title: ${String(error)}`,"error");}});
  const dialog=document.querySelector<HTMLDialogElement>("#management-dialog"); const open=(kind:"rename"|"delete"|"restart",id:string)=>{if(managementBusy)return;management={kind,id,title:""};const record=recordingState.recent.find(item=>item.recording_id===id);const input=document.querySelector<HTMLInputElement>("#management-name")!;input.value=record?.meeting_title??"Meeting";input.closest("label")!.toggleAttribute("hidden",kind!=="rename");document.querySelector("#management-detail")!.textContent=kind==="delete"?"Remove from this list? Audio remains recoverable on disk.":kind==="restart"?"Archive previous transcript output and queue a new transcript with "+settings.whisper_model+"?":"Change the meeting title.";document.querySelector("#management-error")!.textContent="";if(!dialog?.open)dialog?.showModal();if(kind==="rename")input.focus()};
  document.querySelectorAll<HTMLButtonElement>("[data-rename]").forEach(button=>button.onclick=()=>open("rename",button.dataset.rename!));document.querySelectorAll<HTMLButtonElement>("[data-delete]").forEach(button=>button.onclick=()=>open("delete",button.dataset.delete!));document.querySelectorAll<HTMLButtonElement>("[data-restart]").forEach(button=>button.onclick=()=>open("restart",button.dataset.restart!));
  const cancel=document.querySelector<HTMLButtonElement>("#management-cancel")!;cancel.onclick=event=>{event.preventDefault();if(managementBusy)return;management=null;dialog?.close()};if(dialog)dialog.oncancel=event=>{if(managementBusy)event.preventDefault();else management=null};
  document.querySelector<HTMLButtonElement>("#management-confirm")!.onclick=async event=>{event.preventDefault();if(!management||managementBusy)return;const task=management;managementBusy=true;const confirm=document.querySelector<HTMLButtonElement>("#management-confirm")!;confirm.disabled=true;cancel.disabled=true;try{if(task.kind==="rename")await invoke("rename_recording",{recordingId:task.id,title:document.querySelector<HTMLInputElement>("#management-name")!.value});else await invoke(task.kind==="delete"?"delete_recording":"restart_transcription",{recordingId:task.id});dialog?.close();management=null;await refreshRecordingState(true);await refreshTranscriptionState()}catch(error){document.querySelector("#management-error")!.textContent=String(error)}finally{managementBusy=false;confirm.disabled=false;cancel.disabled=false}};
}
async function refreshRecordingState(force = false) {
  if (preview || !backendReady || document.hidden) return;
  if (!force && recordingState.status !== "recording" && Date.now() - lastIdleRecordingPoll < 1000) return;
  if (recordingState.status !== "recording") lastIdleRecordingPoll = Date.now();
  if (recordingActionInFlight && !force) return;
  if (recordingPollPromise) {
    await recordingPollPromise;
    if (!force) return;
  }
  const epoch = recordingEpoch;
  const request = (async () => {
    try {
      const next = await invoke<RecordingState>("get_recording_state");
      if (epoch !== recordingEpoch || recordingActionInFlight) return;
      recordingState = next;
      updateRecordingUI();
    } catch (error) {
      if (epoch !== recordingEpoch || recordingActionInFlight) return;
      recordingState = { ...recordingState, error: `Could not check recording status: ${String(error)}` };
      updateRecordingUI();
    }
  })();
  recordingPollPromise = request;
  try { await request; } finally { if (recordingPollPromise === request) recordingPollPromise = null; }
}
function render() {
  document.querySelector<HTMLDivElement>("#app")!.innerHTML =
    /* HTML */ `<aside>
        <div class="brand">
          ${icon}<span>meeting<br /><b>recorder</b></span>
        </div>
        <nav>
          <button class="active" data-page="dashboard">
            <span>Dashboard</span></button
          ><button data-page="settings"><span>Settings</span></button>
        </nav>
        <div class="side-note">
          Local-first by design<br /><small>No account required</small>
        </div>
      </aside>
      <main>
        ${preview ? `<div class="preview">Browser preview only. Desktop actions and saving are disabled.</div>` : ""}${!preview && !backendReady ? `<button id="retry" class="preview">Retry desktop connection</button>` : ""}
        <div class="topbar">
          <span>YOUR WORKSPACE</span
          ><span id="top-recording-status" class="status-dot">${recordingLabel(recordingState.status)}</span>
        </div>
        <section id="dashboard" class="page">
          <div class="hero">
            <div>
              <p class="kicker">MEETING RECORDER</p>
              <h1>Keep the conversation,<br /><em>not the busywork.</em></h1>
              <p class="lede">
                A calm, private place for your meeting notes and transcripts.
              </p>
            </div>
            <div class="hero-art"><span class="rings"></span>${icon}</div>
          </div>
          <div class="notice">
            <span>Info</span>
            <div>
              <strong>Your recordings stay on this computer.</strong><br /><small
                >Microphone and system audio remain on this device. Make sure everyone has consent before you start.${state.tray_error ? ` ${escapeHtml(state.tray_error)}` : ""}</small
              >
            </div>
          </div>
          <div class="action-row">
            <button id="record" class="primary" ${disabled() || !state.recording_supported ? "disabled" : ""}>
              Start recording</button
            ><button id="stop-recording" class="primary stop-recording" hidden disabled>Stop recording</button
            ><button id="folder" class="secondary" ${disabled()}>
              Open recordings folder</button
            ><button id="quit" class="quiet" ${preview ? "disabled" : ""}>
              Quit
            </button>
          </div>
          <div class="capabilities">
            <article>
              <span class="cap-dot"></span>
              <p>Audio</p>
              <b id="audio-summary">${escapeHtml(sourceSummary(settings.audio_sources))}</b
              ><small>Saved sources are used when you start a recording.</small>
            </article>
            <article>
              <span class="cap-dot"></span>
              <p>Processing</p>
              <b>Local transcription</b
              ><small
                >Whisper runs on this computer. Import a model in Settings.</small
              >
            </article>
            <article>
              <span class="cap-dot warn"></span>
              <p>AppFlowy</p>
              <b>Disconnected</b
              ><small>Connection and export are not available yet.</small>
            </article>
          </div>
          <section class="recording-panel" aria-labelledby="recording-heading">
            <div><p class="kicker">CAPTURE</p><h2 id="recording-heading">Recording</h2></div>
            <div class="recording-readout"><b id="recording-timer" aria-live="off">00:00</b><span id="recording-status" role="status" aria-live="polite">${recordingLabel(recordingState.status)}</span></div>
            <p id="active-sources">${escapeHtml(sourceSummary(settings.audio_sources))}</p><div id="live-meters"></div>
            <p id="recording-error" class="recording-error" role="alert" aria-live="assertive"></p>
          </section>
          <section id="detection-panel" class="detection-panel" aria-labelledby="detection-status">
            <div class="detection-heading"><div><p class="kicker">MEETING DETECTION</p><h3 id="detection-status" role="status">${escapeHtml(detectionLabel())}</h3></div><span id="detection-countdown" aria-live="off"></span></div>
            <p id="detection-detail">${escapeHtml(detectionState.detail)}</p>
            <p id="detection-error" class="recording-error" role="alert"></p>
            <div id="detection-prompt" hidden><p>No audio is recorded until you approve. Make sure participants have consented.</p><div class="detection-actions"><button id="approve-meeting" class="primary">Record this meeting</button><button id="dismiss-meeting" class="secondary">Not this meeting</button></div></div>
          </section>
          <div class="section-head">
            <div>
              <p class="kicker">RECENT</p>
              <h2>Meetings</h2>
            </div>
            <span>Local recordings</span>
          </div>
          <section class="transcription-panel"><p class="kicker">PROCESSING</p><h2>Local transcription</h2><p>Queued recordings process locally after capture. Recording can continue while this runs.</p><div id="transcription-jobs"><small>No recordings are queued.</small></div></section>
          <input id="history-search" aria-label="Search recent meetings" placeholder="Search recent meetings by title or date" value="${escapeAttr(historyQuery)}"/><div id="recent-recordings">${recentRecordings()}</div>
        </section>
        <section id="settings" class="page hidden">
          <p class="kicker">PREFERENCES</p>
          <h1>Settings</h1>
          <p class="lede">Saved locally on this computer.</p>
          <form id="settings-form">
            <label class="toggle"
              ><input
                type="checkbox"
                name="auto_record"
                ${settings.auto_record ? "checked" : ""}
                ${disabled()}
              /><span
                ><b>Detect meetings and ask to record</b
                ><small id="detection-settings-help"
                  >Asks before each recording. Manual recording is always available.</small
                ></span
              ></label
            ><label
              >Start debounce
              <span class="value" data-for="start_debounce_seconds"
                >${settings.start_debounce_seconds} seconds</span
              ><input
                name="start_debounce_seconds"
                type="range"
                min="0"
                max="60"
                value="${settings.start_debounce_seconds}"
                ${disabled()} /></label
            ><label
              >End grace period
              <span class="value" data-for="end_grace_seconds"
                >${settings.end_grace_seconds} seconds</span
              ><input
                name="end_grace_seconds"
                type="range"
                min="5"
                max="300"
                step="5"
                value="${settings.end_grace_seconds}"
                ${disabled()} /></label
            ><label
              >Whisper model<select name="whisper_model" ${disabled()}>
                ${(["tiny", "base", "small", "medium", "large", "turbo"] as Model[]).map((x) => `<option value="${x}" ${settings.whisper_model === x ? "selected" : ""}>${x}${x === "turbo" ? " (recommended)" : ""}${(transcriptionState.imported_models ?? []).includes(x) ? "" : " (not imported)"}</option>`).join("")}</select
              ><small
                >Used for local transcription after you choose an installed model.</small
              ><small id="model-select-warning" role="alert">${(transcriptionState.imported_models ?? []).includes(settings.whisper_model) ? "" : `${settings.whisper_model} is not imported yet; jobs will wait until you import it or pick an imported size.`}</small
              ><small
                >Recommended: <code>turbo</code> (large-v3-turbo), quantized — e.g. <code>ggml-large-v3-turbo-q5_0.bin</code> (~550 MB) from the whisper.cpp Hugging Face repo. It is far more accurate than <code>base</code> for a similar CPU cost.</small
              ></label
            ><label
              >Transcription language<select name="transcription_language" ${disabled()}>
                ${([
                  ["auto", "Detect automatically"],
                  ["en", "English"],
                  ["fr", "French"],
                  ["es", "Spanish"],
                  ["de", "German"],
                  ["it", "Italian"],
                  ["pt", "Portuguese"],
                  ["nl", "Dutch"],
                ] as [Language, string][]).map(([value, label]) => `<option value="${value}" ${(settings.transcription_language ?? "auto") === value ? "selected" : ""}>${label}</option>`).join("")}</select
              ><small
                >"Detect automatically" identifies each audio source's language once, from its first speech, and keeps using it for the rest of the meeting.</small
              ></label
            ><label class="toggle"
              ><input type="checkbox" name="echo_cancellation" ${settings.echo_cancellation !== false ? "checked" : ""} ${disabled()}/><span
                ><b>Cancel speaker echo</b><small
                  >When a recording has both a microphone and a system-audio (loopback) source, removes remote participants' audio picked up a second time by the microphone before transcribing, so it does not appear twice. Has no effect on recordings with only one of those.</small
                ></span
              ></label
            ><label class="toggle"
              ><input type="checkbox" name="dedupe_echo_segments" ${settings.dedupe_echo_segments !== false ? "checked" : ""} ${disabled()}/><span
                ><b>Remove duplicate echoed lines</b><small
                  >After transcribing, drops a microphone line that closely matches an overlapping loopback line -- a text-level safety net that also helps when echo cancellation is off or does not fully catch it.</small
                ></span
              ></label
            >
            <section class="model-settings"><b>Local transcription model</b><p id="model-status">No local model selected</p><p id="backend-status">Backend: ${escapeHtml(transcriptionState.backend)}</p><button type="button" class="secondary" id="choose-model" ${disabled()}>Choose local Whisper model</button><small>Choose a GGML .bin file; model and audio stay local. No model downloads occur automatically.</small></section>
            <section class="model-settings"><label class="toggle"><input type="checkbox" name="diarization_enabled" ${settings.diarization_enabled !== false ? "checked" : ""} ${disabled()}/><span><b>Identify speakers locally</b><small>Labels uncertain turns as unknown. Speaker labels are kept within each audio source and never infer people’s names.</small></span></label><p id="diarization-status">Choose both local models to identify speakers. Transcription remains available.</p><div><button type="button" class="secondary" data-diarization-kind="segmentation" ${disabled()}>Choose segmentation model</button> <button type="button" class="secondary" data-diarization-kind="embedding" ${disabled()}>Choose voice model</button></div></section>
            <section class="model-settings"><label class="toggle"><input type="checkbox" name="summary_enabled" ${settings.summary_enabled !== false ? "checked" : ""} ${disabled()}/><span><b>Summarize meetings locally with Ollama</b><small>Sends the finished transcript to a local Ollama server only (never the internet) for a title, summary, key points, decisions, action items, and open questions. Harmless when Ollama isn't running — summaries simply wait.</small></span></label>
              <label>Ollama URL<input type="text" name="ollama_url" value="${escapeAttr(settings.ollama_url ?? "http://127.0.0.1:11434")}" ${disabled()}/><small>Must be loopback (localhost/127.0.0.1/[::1]) so summaries can never leave this device.</small></label>
              <label>Summary model<select name="summary_model" ${disabled()}>${[...new Set([...summaryModels, settings.summary_model ?? "qwen3:8b"])].map((name) => `<option value="${escapeAttr(name)}" ${(settings.summary_model ?? "qwen3:8b") === name ? "selected" : ""}>${escapeHtml(name)}</option>`).join("")}</select></label>
              <p id="summary-models-status">${escapeHtml(summaryModelsError ?? "Refresh to list models installed in Ollama.")}</p>
              <div><button type="button" class="secondary" id="refresh-summary-models" ${disabled()}>Refresh models</button> <button type="button" class="secondary" id="test-ollama-connection" ${disabled()}>Test connection</button></div>
            </section>
            <section class="model-settings"><label class="toggle"><input type="checkbox" name="compress_audio" ${settings.compress_audio !== false ? "checked" : ""} ${disabled()}/><span><b>Compress finished recordings to FLAC</b><small>Once transcription (and speaker identification, if enabled) has finished with a recording's audio, losslessly compresses each WAV track to FLAC and verifies it decodes back to the exact original samples before removing the WAV. Never runs on an active recording.</small></span></label>
              <p id="audio-storage-total">${escapeHtml(audioStorageSummaryText())}</p>
            </section>
            <section class="audio-settings" aria-labelledby="audio-sources-heading">
              <div class="audio-head"><div><b id="audio-sources-heading">Audio sources</b><small>Select one or more inputs for local capture.</small></div><button type="button" id="refresh-devices" class="secondary" ${disabled()}>Refresh</button></div>
              <p id="capture-next" class="capture-next">Select one or more sources for your next local recording.</p>
              <div id="device-list">${deviceRows()}</div><p id="inventory-backend" class="inventory-backend">${inventory?.backend ? `Backend: ${escapeHtml(inventory.backend)}` : "Backend unavailable"}</p><p id="device-errors" class="device-errors" role="alert" aria-live="assertive"></p><button class="primary" type="button" id="save-audio-sources" ${disabled()}>Save audio sources</button>
            </section>
            <div class="integration">
              <b>AppFlowy</b><span>Disconnected</span
              ><small>Connection and exports are not available yet.</small>
            </div>
            <button class="primary" type="submit" ${disabled()}>
              Save preferences
            </button>
            <p class="form-message" role="status"></p>
          </form>
        </section>
        <dialog id="management-dialog" aria-labelledby="management-title"><form method="dialog"><h2 id="management-title">Manage meeting</h2><label id="management-name-label">Meeting title<input id="management-name" /></label><p id="management-detail"></p><p id="management-error" role="alert"></p><menu><button id="management-cancel" value="cancel">Cancel</button><button id="management-confirm" value="default">Confirm</button></menu></form></dialog><p id="toast" role="alert" aria-live="polite"></p>
      </main>`;
  document.querySelectorAll<HTMLButtonElement>("[data-page]").forEach(
    (button) =>
      (button.onclick = () => {
        document
          .querySelectorAll(".page")
          .forEach((page) => page.classList.add("hidden"));
        document
          .getElementById(button.dataset.page!)!
          .classList.remove("hidden");
        document
          .querySelectorAll("nav button")
          .forEach((item) => item.classList.remove("active"));
        button.classList.add("active");
      }),
  );
  document.querySelectorAll<HTMLInputElement>("input[type=range]").forEach(
    (input) =>
      (input.oninput = () => {
        const label = document.querySelector<HTMLElement>(
          `[data-for='${input.name}']`,
        );
        if (label) label.textContent = `${input.value} seconds`;
      }),
  );
  document.querySelector("#record")?.addEventListener("click", async () => {
    if (recordingActionInFlight) return;
    if (!settings.audio_sources.length) {
      notice("Choose and save at least one audio source before recording.", "error");
      document.querySelector<HTMLButtonElement>("[data-page='settings']")?.click();
      return;
    }
    recordingEpoch++;
    recordingActionInFlight = true;
    updateRecordingUI();
    try {
      await invoke("start_recording");
    } catch (error) {
      notice(`Could not start recording: ${String(error)}`, "error");
    } finally {
      recordingActionInFlight = false;
      await refreshRecordingState(true);
      updateRecordingUI();
    }
  });
  document.querySelector("#stop-recording")?.addEventListener("click", async () => {
    if (recordingActionInFlight) return;
    recordingEpoch++;
    recordingActionInFlight = true;
    updateRecordingUI();
    try {
      await invoke("stop_recording");
    } catch (error) {
      notice(`Could not stop recording: ${String(error)}`, "error");
    } finally {
      recordingActionInFlight = false;
      await refreshRecordingState(true);
      updateRecordingUI();
    }
  });
  document
    .querySelector("#folder")
    ?.addEventListener("click", () =>
      invoke("open_recordings_folder").catch((error) =>
        notice(String(error), "error"),
      ),
    );
  document
    .querySelector("#quit")
    ?.addEventListener("click", () =>
      invoke("quit_application").catch((error) =>
        notice(String(error), "error"),
      ),
    );
  document.querySelector("#retry")?.addEventListener("click", loadDesktop);
  for (const [id, command] of [["approve-meeting", "confirm_detected_recording"], ["dismiss-meeting", "dismiss_detected_recording"]]) {
    document.getElementById(id)?.addEventListener("click", async () => {
      const approvalId = detectionState.approval_id;
      if (detectionActionInFlight || !approvalId || detectionState.phase !== "awaiting_approval") return;
      detectionActionInFlight = true;
      updateDetectionUI();
      let actionError: string | null = null;
      try {
        await invoke(command, { approvalId });
      } catch (error) {
        actionError = `Could not update meeting request: ${String(error)}`;
        notice(actionError, "error");
      } finally {
        detectionActionInFlight = false;
        await refreshDetectionState();
        // A stale approval is an action error, not a reason to manufacture a
        // recording state. Keep it visible until the next detector sample.
        if (actionError) detectionState = { ...detectionState, error: actionError };
        await refreshRecordingState(true);
        updateDetectionUI();
      }
    });
  }
  document.querySelector("#refresh-devices")?.addEventListener("click", refreshDevices);
  renderedDeviceRows = deviceRows();
  renderedRecentRecordings = recentRecordings();
  refreshDeviceSection();
  updateDetectionUI();
  updateTranscriptionUI();
  updateSummarySettingsUI();
  document.querySelector("#choose-model")?.addEventListener("click", async () => {
    const button = document.querySelector<HTMLButtonElement>("#choose-model");
    if (button) button.disabled = true;
    try {
      const result=await invoke<{key:string;switched_to?:string|null}|null>("import_whisper_model");
      if (result) {
        if (result.switched_to) {
          settings = { ...settings, whisper_model: result.switched_to as Model };
          const select = document.querySelector<HTMLSelectElement>('select[name="whisper_model"]');
          if (select) select.value = result.switched_to;
        }
        notice(result.switched_to ? `Imported ${result.key}. Switched your selected model to ${result.key} so it's used right away.` : result.key === settings.whisper_model ? `Imported ${result.key}. Ready for local transcription.` : `Imported ${result.key}. Select ${result.key} above and save preferences to use it for new jobs.`);
      }
      await refreshTranscriptionState();
    }
    catch (error) { notice(`Could not choose local model: ${String(error)}`, "error"); }
    finally { if (button) button.disabled = preview || !backendReady; }
  });
  document.querySelectorAll<HTMLButtonElement>("[data-diarization-kind]").forEach(button=>button.onclick=async()=>{button.disabled=true;try{const result=await invoke<{key:string}|null>("import_diarization_model",{kind:button.dataset.diarizationKind});if(result)notice(`Imported ${result.key} locally.`);await refreshDiarizationState()}catch(error){notice(`Could not choose local speaker model: ${String(error)}`,"error")}finally{button.disabled=preview||!backendReady}});
  document.querySelector<HTMLFormElement>("#settings-form")!.onsubmit = async (
    event,
  ) => {
    event.preventDefault();
    const form = new FormData(event.currentTarget as HTMLFormElement);
    const next: Settings = {
      auto_record: form.get("auto_record") === "on",
      start_debounce_seconds: Number(form.get("start_debounce_seconds")),
      end_grace_seconds: Number(form.get("end_grace_seconds")),
      whisper_model: String(form.get("whisper_model")) as Model,
      diarization_enabled: form.get("diarization_enabled") === "on",
      transcription_language: String(form.get("transcription_language") ?? "auto") as Language,
      echo_cancellation: form.get("echo_cancellation") === "on",
      dedupe_echo_segments: form.get("dedupe_echo_segments") === "on",
      summary_enabled: form.get("summary_enabled") === "on",
      ollama_url: String(form.get("ollama_url") ?? "http://127.0.0.1:11434").trim(),
      summary_model: String(form.get("summary_model") ?? "qwen3:8b"),
      compress_audio: form.get("compress_audio") === "on",
      audio_sources: settings.audio_sources,
    };
    try {
      const saved = await invoke<Settings>("save_settings", { settings: next });
      settings = { ...saved, audio_sources: saved.audio_sources.map((source) => draftSources.get(source.id) ?? source) };
      notice("Preferences saved locally.");
      refreshDetectionState();
      refreshTranscriptionState();
      refreshDiarizationState();
      refreshSummaryState();
    } catch (error) {
      notice(`Could not save preferences: ${String(error)}`, "error");
    }
  };
  document.querySelector("#refresh-summary-models")?.addEventListener("click", () => {
    const url = document.querySelector<HTMLInputElement>('input[name="ollama_url"]')?.value ?? settings.ollama_url ?? "http://127.0.0.1:11434";
    refreshSummaryModels(url);
  });
  document.querySelector("#test-ollama-connection")?.addEventListener("click", async () => {
    const button = document.querySelector<HTMLButtonElement>("#test-ollama-connection")!;
    const url = document.querySelector<HTMLInputElement>('input[name="ollama_url"]')?.value ?? settings.ollama_url ?? "http://127.0.0.1:11434";
    button.disabled = true;
    try {
      await invoke("test_ollama_connection", { ollamaUrl: url });
      notice("Connected to Ollama.");
    } catch (error) {
      notice(`Could not connect to Ollama: ${String(error)}`, "error");
    } finally {
      button.disabled = preview || !backendReady;
    }
  });
  document.querySelector("#save-audio-sources")?.addEventListener("click", async () => {
    if (audioSaveInFlight) return;
    audioSaveInFlight = true;
    refreshDeviceSection();
    const button = document.querySelector<HTMLButtonElement>("#save-audio-sources");
    if (button) button.disabled = true;
    try {
      const saved = await invoke<Settings>("save_audio_selection", { sourceIds: [...selectedSourceIds] });
      settings = {
        ...saved,
        audio_sources: saved.audio_sources.map((source) => draftSources.get(source.id) ?? source),
      };
      selectedSourceIds = new Set(settings.audio_sources.map((source) => source.id));
      draftSources = new Map(settings.audio_sources.map((source) => [source.id, source]));
      notice("Audio sources saved locally for your next recording.");
      const summary = document.querySelector<HTMLElement>("#audio-summary");
      if (summary) summary.textContent = sourceSummary(settings.audio_sources);
    } catch (error) {
      notice(`Could not save audio sources: ${String(error)}`, "error");
    } finally {
      audioSaveInFlight = false;
      if (button) button.disabled = preview || !backendReady;
      refreshDeviceSection();
    }
  });
}
if (!preview) {
  listen("detection-state", () => {
    refreshDetectionState();
    // Navigate without rendering again, preserving any unsaved settings draft.
    document.querySelector<HTMLButtonElement>("[data-page='dashboard']")?.click();
  });
  listen("recording-unavailable", () => notice("Recording could not start. No audio was captured.", "error"));
  listen<string>("recording-failed", (event) => {
    notice(`Recording error: ${event.payload}`, "error");
    // A close request can have hidden the app before background finalization
    // discovers an error. Bring the error into the user's normal polling UI.
    refreshRecordingState();
  });
  listen("navigate-settings", () =>
    document
      .querySelector<HTMLButtonElement>("[data-page='settings']")
      ?.click(),
  );
  loadDesktop();
  devicePoll = window.setInterval(refreshDevices, 5000);
  recordingPoll = window.setInterval(()=>refreshRecordingState(), 100);
  window.setInterval(refreshTranscriptionState, 1000);
  window.setInterval(refreshDiarizationState, 1000);
  window.setInterval(refreshSummaryState, 1000);
  window.setInterval(refreshCompressionState, 1000);
  window.setInterval(refreshAudioStorage, 5000);
  window.setInterval(refreshDetectionState, 1000);
  window.addEventListener("focus", refreshDevices);
  window.addEventListener("focus", () => { refreshRecordingState(); refreshTranscriptionState(); refreshDiarizationState(); refreshSummaryState(); refreshCompressionState(); refreshAudioStorage(); });
  window.addEventListener("focus", refreshDetectionState);
  document.addEventListener("visibilitychange", () => { if (!document.hidden) { refreshDevices(); refreshRecordingState(); refreshDetectionState(); refreshTranscriptionState(); refreshSummaryState(); refreshCompressionState(); refreshAudioStorage(); } });
} else render();
