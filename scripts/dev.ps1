param(
  # Build with GPU (CUDA) transcription support. Requires the CUDA Toolkit
  # (nvcc, headers, libs) to be installed; see docs/transcription.md.
  [switch]$Cuda
)

$projectRoot = Split-Path -Parent $PSScriptRoot
$localCargo = Join-Path $projectRoot '.tools\cargo\bin'
$localRustup = Join-Path $projectRoot '.tools\rustup'

if (Test-Path (Join-Path $localCargo 'cargo.exe')) {
  $env:CARGO_HOME = Join-Path $projectRoot '.tools\cargo'
  $env:RUSTUP_HOME = $localRustup
  $env:PATH = "$localCargo;$env:PATH"
}

$localLibclang = Join-Path $projectRoot '.tools\libclang\clang\native'
if (Test-Path (Join-Path $localLibclang 'libclang.dll')) {
  $env:LIBCLANG_PATH = $localLibclang
}

$npmArgs = @('run', 'tauri', 'dev')

if ($Cuda) {
  if (-not $env:CUDA_PATH -or -not (Test-Path $env:CUDA_PATH)) {
    Write-Warning "CUDA_PATH is not set or does not exist; the cuda feature build will fail without the CUDA Toolkit installed. See docs/transcription.md."
  } else {
    # Put the CUDA Toolkit's bin (nvcc, and at runtime cudart/cublas DLLs) on
    # PATH for both the CMake build and the running app.
    $cudaBin = Join-Path $env:CUDA_PATH 'bin'
    if (Test-Path $cudaBin) {
      $env:PATH = "$cudaBin;$env:PATH"
    }
  }
  $npmArgs += @('--', '--features', 'cuda')
}

Push-Location $projectRoot
try { npm @npmArgs } finally { Pop-Location }
