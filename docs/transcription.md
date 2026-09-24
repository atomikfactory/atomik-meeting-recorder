# Local transcription

## Scope and privacy

Whisper runs inside a single Rust background worker. Audio, model files, JSON checkpoints, and Markdown stay on this device. There are no runtime model downloads, external inference APIs, telemetry, summaries, or AppFlowy uploads in this increment. AppFlowy Cloud remains a later phase; optional speaker identification is documented separately in [diarization.md](diarization.md).

New recordings have an explicit `auto_transcribe` metadata flag. Missing flags deserialize to false: earlier hardware checks and legacy recordings are never automatically processed. The UI cannot submit arbitrary audio paths. Retry accepts only an existing job's recording ID; opening a transcript verifies its owned paths and saved contents.

## Models

Use Settings → Choose local Whisper model to import a trusted GGML `.bin` file, then select its size and save preferences. `base` is the multilingual default; tiny/base/small/medium/large are recognized from model dimensions. Large-v3-turbo and other nonstandard layer layouts are not supported by this initial importer. English-only variants occupy the same size slot. No model is bundled.

The official [whisper.cpp model instructions](https://github.com/ggml-org/whisper.cpp/blob/master/models/README.md) link available files. Prefer that upstream distribution over unknown model files. A manual download is network access made by the user, not the application. The upstream multilingual base file was verified through repository metadata as 147,951,465 bytes with SHA-256 `60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe`.

Import validates the GGML header and size, hashes the file, copies it to `recordings/models/<sha256>.bin`, verifies the copy, and registers it in SQLite. Existing hash-named files are checked, not silently overwritten. Header validation does not prove all model weights are intact; the decoder performs full loading at transcription time and reports failures. Import only models you trust.

Changing the selected size affects newly queued jobs, and also re-points any job that has not yet been claimed (still `waiting_model`/`queued`) to the newly selected size, so it is not stuck on a size that was never imported. Once claimed, a job's exact model hash/path is pinned through retries and restarts regardless of later setting changes. Importing a model also switches the saved setting to it automatically when the previously selected size was not imported, so importing one model is enough to get transcription working. Large models can require substantial RAM even with four CPU threads; begin with base or tiny.

## Processing and output

The engine uses [whisper-rs 0.16](https://docs.rs/whisper-rs/0.16.0/whisper_rs/) and its bundled whisper.cpp, [Rubato 5](https://docs.rs/rubato/5.0.0/rubato/) for anti-aliased 16 kHz resampling, and Hound for PCM16 WAV decoding. Each source is downmixed independently and processed in bounded 60-second chunks. The model loads once per recording. Progress is chunk-based, not an estimated time remaining. Quit requests cancellation and waits off the UI thread for the worker.

For each recording:

```text
recordings/<recording-id>/
  track-00.wav
  track-01.wav
  transcript.json
  transcript.md
```

Markdown includes the recording's UTC start date/time, duration, stable ID, model size, audio sources, timestamps, and capture/processing warnings. Passage text is escaped as literal Markdown so recognized text cannot silently embed external images or executable HTML. No summary is generated.

Device labels are **not speaker labels**. Multiple sources can contain overlapping or duplicate speech. There is no acoustic echo cancellation, cross-device drift correction, speaker recognition, or cross-track speech deduplication. Chunk boundaries can split/repeat words. Whisper may misrecognize speech or hallucinate on nonspeech; exact digital silence is skipped but this is not a complete voice-activity detector. Captures with retained gaps can be transcribed and remain labelled incomplete.

## Durable states

`transcription.sqlite3` is separate from capture metadata. One job per recording ID transitions from queued → waiting_model or transcribing → completed/failed. Claims are conditional; attempts, model identity, artifact paths, and transcript hash persist. Failed jobs do not spin in an automatic retry loop: use Retry transcription.

JSON is flushed and renamed first, then Markdown, then the completion transaction. On restart, abandoned claims are requeued. If a valid matching JSON checkpoint exists, Markdown is completed without rerunning Whisper. Corrupt/mismatched checkpoints and conflicting Markdown fail closed and stay on disk for inspection. Completed jobs are never automatically retranscribed, including when an artifact is missing. The Open transcript action checks integrity and reports missing/modified files; editing Markdown yourself does not cause the app to overwrite it.

The application is not a backup system. Keep backups of the recordings directory; native filesystem durability across sudden power loss still requires platform testing.

## Build and verification

Whisper requires a C++ toolchain, CMake, and libclang at build time; no Python interpreter or libclang is shipped as an application runtime dependency. `LIBCLANG_PATH` must point to the directory containing the appropriate libclang shared library. Do **not** set `WHISPER_DONT_GENERATE_BINDINGS=1` on Windows: this locked sys crate's bundled bindings contain Linux-specific types.

On this development host, [libclang 18.1.1](https://pypi.org/project/libclang/18.1.1/) was installed only into `.tools/libclang` using `python -m pip install --target .tools/libclang --no-deps --only-binary=:all: libclang==18.1.1`. The convenience `scripts/dev.ps1` discovers its DLL without changing system configuration. Other developers can use their normal LLVM installation.

`.cargo/config.toml` disables host-native and AVX-family build flags for a conservative baseline. This favors portability over speed; native performance must be measured before promising real-time throughput. Native macOS/Linux builds and permissions remain unverified on this Windows host. CI includes all three targets but a configured matrix is not evidence of a completed run.

Synthetic tests use a fake engine and isolated files, never existing microphone recordings. The ignored `transcribes_explicit_public_fixture` test requires explicitly supplied `WHISPER_TEST_MODEL` and `WHISPER_TEST_AUDIO`; it makes no downloads. Do not point it at private recordings without separate consent.

For native UI checks set **both** `MEETING_RECORDER_DISABLE_AUTOMATION=1` and `MEETING_RECORDER_DISABLE_TRANSCRIPTION=1`. The second prevents audio reads/job claims, including after restart. Model import alone does not override that inhibition.
