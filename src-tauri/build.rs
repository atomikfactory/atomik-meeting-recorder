fn main() {
    tauri_build::build();
    #[cfg(all(windows, feature = "cuda"))]
    copy_cuda_redistributables();
    #[cfg(windows)]
    copy_native_dlls_beside_test_binaries();
}

/// sherpa-onnx-sys's build script (a build-order dependency, so it always runs
/// before this one) copies its shared DLLs -- notably `onnxruntime.dll` --
/// beside the compiled *application* executable at `target/<profile>/`. That is
/// exactly right for the shipped app, whose own exe lives there and so wins
/// Windows's "search the executable's own directory first" rule even against a
/// same-named DLL elsewhere (e.g. `System32`). `cargo test` binaries, though,
/// live one level down at `target/<profile>/deps/`, which is outside that
/// search path; implicit DLL dependencies resolve at process image load, before
/// any of the test binary's own code runs, so a copy attempted from inside a
/// test is always too late once the wrong DLL has already been mapped in. This
/// was observed concretely on a real machine: it has an unrelated, older
/// `onnxruntime.dll` in `System32` (apparently from a Windows ML/Copilot
/// Runtime component), and without this fixup, sherpa-onnx's native diarization
/// calls immediately crash with an ONNX Runtime API-version mismatch
/// (`STATUS_ACCESS_VIOLATION`) when run via `cargo test`. Copying the correct
/// DLLs into `deps/` here, during this build script (which cargo always runs
/// before it runs any test binary in the same invocation), fixes that for good.
#[cfg(windows)]
fn copy_native_dlls_beside_test_binaries() {
    use std::path::PathBuf;

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    let Some(profile_dir) = out_dir.ancestors().nth(3) else {
        return;
    };
    let deps_dir = profile_dir.join("deps");
    if let Err(err) = std::fs::create_dir_all(&deps_dir) {
        println!(
            "cargo:warning=Could not create {}: {err}",
            deps_dir.display()
        );
        return;
    }
    let Ok(entries) = std::fs::read_dir(profile_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("dll") {
            continue;
        }
        let dest = deps_dir.join(path.file_name().expect("dll path has a file name"));
        // Best-effort: a locked destination (e.g. a running test binary that
        // has this DLL loaded) must never fail the whole build.
        let _ = std::fs::copy(&path, &dest);
    }
}

/// The `cuda` feature links whisper.cpp's ggml-cuda backend against the CUDA
/// Toolkit's import libraries, but at *runtime* the process only needs the
/// redistributable CUDA DLLs (cudart, cublas, cublasLt) -- not the full
/// Toolkit. Sherpa-onnx's own build script already copies its shared DLLs
/// beside the executable (see the `sherpa-onnx` dependency comment in
/// Cargo.toml); this mirrors that so a CUDA release build is bundlable
/// without requiring the Toolkit on the target machine. `nvcuda.dll` itself
/// comes from the NVIDIA display driver and must never be copied here.
#[cfg(all(windows, feature = "cuda"))]
fn copy_cuda_redistributables() {
    use std::path::{Path, PathBuf};

    let Ok(cuda_path) = std::env::var("CUDA_PATH") else {
        println!("cargo:warning=CUDA_PATH not set; skipping CUDA redistributable DLL copy");
        return;
    };
    let bin_dir = {
        let x64 = Path::new(&cuda_path).join("bin").join("x64");
        if x64.is_dir() {
            x64
        } else {
            Path::new(&cuda_path).join("bin")
        }
    };

    // OUT_DIR is target/<profile>/build/<pkg>-<hash>/out; the executable
    // lives three levels up, at target/<profile>.
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    let target_dir = out_dir
        .ancestors()
        .nth(3)
        .expect("OUT_DIR has at least 3 ancestors")
        .to_path_buf();

    const REDISTRIBUTABLE_DLLS: &[&str] =
        &["cudart64_13.dll", "cublas64_13.dll", "cublasLt64_13.dll"];

    for name in REDISTRIBUTABLE_DLLS {
        let src = bin_dir.join(name);
        if !src.exists() {
            println!(
                "cargo:warning=CUDA redistributable {name} not found under {}; \
                 the cuda-enabled binary may fail to start on machines without the Toolkit",
                bin_dir.display()
            );
            continue;
        }
        let dest = target_dir.join(name);
        println!("cargo:rerun-if-changed={}", src.display());
        if let Err(err) = std::fs::copy(&src, &dest) {
            println!(
                "cargo:warning=Failed to copy {} to {}: {err}",
                src.display(),
                dest.display()
            );
        }
    }
}
