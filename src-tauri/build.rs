fn main() {
    tauri_build::build();
    #[cfg(all(windows, feature = "cuda"))]
    copy_cuda_redistributables();
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
