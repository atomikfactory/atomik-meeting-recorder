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
  # A terminal opened before the toolkit was installed does not see the CUDA
  # variables yet. MSBuild's CUDA targets resolve the toolkit through the
  # versioned CUDA_PATH_Vxx_y, not CUDA_PATH, so import all of them.
  foreach ($entry in [Environment]::GetEnvironmentVariables('Machine').GetEnumerator()) {
    if ($entry.Key -like 'CUDA_PATH*' -and -not (Test-Path "env:$($entry.Key)")) {
      Set-Item "env:$($entry.Key)" $entry.Value
    }
  }
  if (-not $env:CUDA_PATH -or -not (Test-Path (Join-Path $env:CUDA_PATH 'bin\nvcc.exe'))) {
    Write-Error "CUDA Toolkit not found (CUDA_PATH unset or nvcc.exe missing). Install it (e.g. 'winget install Nvidia.CUDA'), or run without -Cuda for the CPU build. See docs/transcription.md." -ErrorAction Continue
    exit 1
  } else {
    # Put the CUDA Toolkit's bin (nvcc, and at runtime cudart/cublas DLLs) on
    # PATH for both the CMake build and the running app.
    # MSBuild reads environment variables as properties; pin the toolkit so the
    # Visual Studio generator never falls back to an empty CudaToolkitDir.
    $env:CudaToolkitDir = $env:CUDA_PATH.TrimEnd('\') + '\'
    # CUDA 13 ships its runtime DLLs in bin\x64; older toolkits use bin.
    foreach ($cudaBin in @((Join-Path $env:CUDA_PATH 'bin'), (Join-Path $env:CUDA_PATH 'bin\x64'))) {
      if (Test-Path $cudaBin) {
        $env:PATH = "$cudaBin;$env:PATH"
      }
    }
  }
  $npmArgs += @('--', '--features', 'cuda')
}

Push-Location $projectRoot
try { npm @npmArgs } finally { Pop-Location }
