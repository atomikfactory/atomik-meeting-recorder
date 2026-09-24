# Local speaker identification

Speaker identification is an optional, separate post-processing stage. It uses trusted local ONNX segmentation and embedding models selected through the native file picker; the application performs no runtime model downloads. The CPU-only Sherpa 1.13.8 helper is isolated from capture and transcription.

Use compatible, approved models you are licensed to use. Sherpa's [official model documentation](https://k2-fsa.github.io/sherpa/onnx/pretrained_models/index.html) is the primary source for compatibility information. Segmentation is intended for a compatible pyannote segmentation 3.0 model and embedding for an approved compatible voice model; the app does not recommend non-commercial Reverb defaults. Build-time helper dependencies are distinct from runtime model downloads.

Jobs are bounded to 30 minutes per audio source. Labels are conservative: uncertain turns are `unknown`, labels apply only within one source, and the app does not infer names or merge identities. A checkpointed companion JSON/Markdown result preserves the original transcript. Retrying diarization does not retry Whisper. Missing speaker models or a diarization failure never blocks opening a completed transcript.

Disabling transcription also inhibits speaker processing. Native diarization stage contracts and real inference are pending verification, including macOS and Linux.

Windows uses Sherpa shared-library companion DLLs; packaged builds must ship those DLLs beside the executable. macOS and Linux use their platform-appropriate static helper linkage.
