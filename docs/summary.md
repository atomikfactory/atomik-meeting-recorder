# Local meeting summaries (Ollama)

An optional, separate post-processing stage, like speaker identification: it starts only after a transcript completes, never blocks transcription, and a missing or unreachable summarizer never hides a completed transcript. It uses a local [Ollama](https://ollama.com) server -- the app never talks to any other summarization service, local or remote.

## Privacy

The Ollama URL setting is validated to be loopback only (`localhost`, `127.0.0.1`, or `[::1]`); any other host is rejected when saving settings, so this feature cannot become a route off this device. The transcript text and a short recording-metadata header (duration, audio source names) are the only things sent, and only to that loopback address. No summary request is retried against a different host, and no model is downloaded automatically -- Ollama and its models are the user's own local install.

## Models

Any model already pulled into the local Ollama install can be selected in Settings (the dropdown lists whatever `ollama list`/`/api/tags` reports); `qwen3:8b` is the default and recommended for its balance of speed and quality, with `qwen2.5:7b-instruct` and other locally available models as alternatives. For `qwen3` models the request sets `think: false`, since this task does not benefit from the extra latency of visible reasoning.

## Output

Two files are written next to a recording's transcript: `summary.json` (title, summary, key points, decisions, action items with optional owner/due, open questions, detected language, plus the model used and a checksum of the transcript it summarized) and a formatted `summary.md`. Both are written atomically and only once -- a job whose transcript later changes (for example after "Restart transcription") is invalidated and re-summarized from the new transcript, and its previous outputs are archived alongside the transcript's own archived outputs.

The summary's language matches the meeting's own language when it is known from transcription (or "Detect automatically" resolved to a single language); otherwise the model is asked to detect it. The prompt explicitly tells the model that transcript source labels ("Microphone", "Speakers"/"System audio") describe the *audio device* a line was captured from, not a speaker's identity or name, and that older transcripts (recorded before acoustic echo cancellation existed) may contain near-duplicate lines across sources from echo -- the model is told not to treat those as two different people saying the same thing. It is also told not to invent a name, owner, or due date that the transcript does not state or clearly imply.

## Long transcripts

A transcript that would not fit in the model's context window is split into overlapping chunks (a few segments of overlap between adjacent chunks, so a decision or action item spanning a chunk boundary is not lost), each summarized into a short narrative, then merged into the final structured summary in one further call. `num_ctx` scales with the transcript's length up to 32768.

## Serialization with Whisper/speaker identification

Summarizing shares the same processing lock as local transcription and speaker identification, so it never runs at the same time as either -- on a single consumer GPU, running an LLM alongside a Whisper decode or diarization pass would contend for the same VRAM.

## Errors and retry

Ollama being unreachable, or not having the selected model pulled yet, is treated as transient: the job moves to a "waiting" state with an explanatory message (including the exact `ollama pull <model>` command when a model is missing) and is retried automatically with exponential backoff, capped at five minutes -- it never needs a manual retry click just because Ollama was not running yet. Saving a settings change that could unblock a waiting job (enabling the feature, or fixing the URL/model) wakes it immediately. Any other failure (a malformed or empty response) lands in a `failed` state with a `Retry summary` action.

## UI

The recording card shows the summary's status alongside transcript/speaker status, with actions to open the finished `summary.md`, regenerate it with the currently selected model, retry a failed job, and apply the model's suggested title with one click (reusing the existing rename action).
