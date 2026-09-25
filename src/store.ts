// Shared mutable application state.
//
// This is a straight port of main.ts's former top-level `let` bindings into
// one object. A module cannot reassign a value it imported (`import { x }`
// gives a read-only live binding), so state that gets *replaced* wholesale
// (e.g. `settings = { ...saved }`) needs a stable container whose fields are
// reassigned instead -- hence `store.settings = ...`. Sets/Maps that are only
// mutated in place (via `.add`/`.delete`) don't have this problem and stay as
// plain module-level `const` exports next to the code that owns them.
import { preview } from "./api";
import type {
  AppFlowyState,
  AppFlowyWorkspace,
  AppState,
  AudioInventory,
  AudioSourceSelection,
  AudioStorageSummary,
  BrowserExtensionStatus,
  CompressionState,
  DetectionState,
  DiarizationState,
  Recording,
  RecordingState,
  SearchResults,
  Settings,
  SummaryState,
  TranscriptionState,
} from "./types";

export type ManagementTask = { kind: "rename" | "delete" | "restart"; id: string; title: string };

export const icon = `<svg viewBox="0 0 48 48" aria-hidden="true"><rect x="2" y="2" width="44" height="44" rx="14" fill="#0d746e"/><rect x="19" y="10" width="10" height="19" rx="5" fill="#f7f4ed"/><path d="M14 24a10 10 0 0 0 20 0M24 34v5M18 39h12" fill="none" stroke="#f7f4ed" stroke-width="3" stroke-linecap="round"/></svg>`;

export const store = {
  backendReady: false as boolean,

  settings: {
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
    local_speaker_name: "You",
    appflowy_enabled: false,
    appflowy_auto_export: false,
    appflowy_base_url: "https://beta.appflowy.cloud",
    appflowy_use_native_block: true,
    appflowy_workspace_id: null,
  } as Settings,

  inventory: null as AudioInventory | null,
  selectedSourceIds: new Set<string>(),
  // Keep a full draft record so unplugged devices retain their last known identity.
  draftSources: new Map<string, AudioSourceSelection>(),
  renderedDeviceRows: null as string | null,
  inventoryRefreshing: false,
  audioSaveInFlight: false,

  recordingActionInFlight: false,
  recordingPollPromise: null as Promise<void> | null,
  recordingEpoch: 0,
  lastIdleRecordingPoll: 0,

  transcriptionState: { inhibited:false, model:{key:"base",available:false}, jobs:[], imported_models:[], error:null, backend:"cpu (AVX2)" } as TranscriptionState,
  transcriptionPollInFlight: false,

  diarizationState: {enabled:true,inhibited:false,models:[],jobs:[],error:null} as DiarizationState,
  diarizationPoll: false,

  summaryState: {enabled:true,inhibited:false,jobs:[],error:null} as SummaryState,
  summaryPoll: false,
  summaryModels: [] as string[],
  summaryModelsError: null as string | null,
  summaryModelsLoading: false,

  appflowyState: {enabled:false,auto_export:false,connected:false,email:null,base_url:"https://beta.appflowy.cloud",jobs:[],inhibited:false,error:null} as AppFlowyState,
  appflowyPoll: false,
  appflowyWorkspaces: [] as AppFlowyWorkspace[],
  appflowyOtpEmail: "",
  appflowyOtpSent: false,
  appflowyAuthBusy: false,
  appflowyAuthError: null as string | null,

  compressionState: {enabled:true,inhibited:false,jobs:[],error:null} as CompressionState,
  compressionPoll: false,

  browserExtensionState: {connected:false,last_message_seconds_ago:null,registered:false,browsers:[]} as BrowserExtensionStatus,
  browserExtensionPoll: false,
  browserExtensionActionInFlight: false,

  audioStorage: {total_bytes:0,total_original_bytes:0,recordings:[]} as AudioStorageSummary,
  audioStoragePoll: false,

  renderedRecentRecordings: null as string | null,
  historyQuery: "",

  // Full-text search across every recorded meeting's transcript and summary
  // (backed by the `search_transcripts` command's SQLite FTS5 index), distinct
  // from `historyQuery`'s client-side filter over just the 25 most recent rows.
  transcriptSearchQuery: "",
  transcriptSearchResults: null as SearchResults | null,
  transcriptSearchLoading: false,
  transcriptSearchError: null as string | null,
  transcriptSearchDebounce: undefined as number | undefined,
  transcriptSearchRequestId: 0,

  management: null as ManagementTask | null,
  managementBusy: false,

  recordingState: { status: "idle", active: null, recent: [], error: null } as RecordingState,

  detectionState: { supported: false, enabled: false, phase: "unsupported", provider: null, detail: "Meeting detection is available in the desktop app on supported platforms.", error: null, seconds_remaining: null, automation_inhibited: false } as DetectionState,
  detectionPollInFlight: false,
  detectionActionInFlight: false,

  appState: {
    status: "idle",
    recording_supported: false,
    detection_supported: false,
    appflowy_connected: false,
    recordings_folder: "Available in the desktop app",
    tray_available: false,
    tray_error: null,
  } as AppState,
};

// Sets mutated only via `.add`/`.delete`, never reassigned -- safe as plain
// module-level exports (see file header).
export const appflowyActionInFlight = new Set<string>();
export const summaryActionInFlight = new Set<string>();
export const transcribeInFlight = new Set<string>();

export function recordingBusy() {
  return store.recordingState.status === "recording" || store.recordingState.status === "processing" || Boolean(store.recordingState.active);
}

export function disabled() {
  return preview || !store.backendReady ? 'disabled aria-disabled="true"' : "";
}

export type { Recording };
