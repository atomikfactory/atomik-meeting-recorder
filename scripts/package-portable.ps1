param(
  # Build the CUDA release binary first (equivalent to
  # `npm run tauri:build:cuda --no-bundle`) before assembling the portable
  # folder. Omit this if you already ran the CUDA release build yourself.
  [switch]$Build
)

$ErrorActionPreference = 'Stop'

$projectRoot = Split-Path -Parent $PSScriptRoot
$srcTauri = Join-Path $projectRoot 'src-tauri'
$releaseDir = Join-Path $srcTauri 'target\release'

if ($Build) {
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
  foreach ($entry in [Environment]::GetEnvironmentVariables('Machine').GetEnumerator()) {
    if ($entry.Key -like 'CUDA_PATH*' -and -not (Test-Path "env:$($entry.Key)")) {
      Set-Item "env:$($entry.Key)" $entry.Value
    }
  }
  if (-not $env:CUDA_PATH -or -not (Test-Path (Join-Path $env:CUDA_PATH 'bin\nvcc.exe'))) {
    Write-Error "CUDA Toolkit not found (CUDA_PATH unset or nvcc.exe missing). See docs/transcription.md." -ErrorAction Continue
    exit 1
  }
  $env:CudaToolkitDir = $env:CUDA_PATH.TrimEnd('\') + '\'
  foreach ($cudaBin in @((Join-Path $env:CUDA_PATH 'bin'), (Join-Path $env:CUDA_PATH 'bin\x64'))) {
    if (Test-Path $cudaBin) {
      $env:PATH = "$cudaBin;$env:PATH"
    }
  }
  Push-Location $projectRoot
  try {
    npm run tauri -- build --features cuda --no-bundle
  } finally {
    Pop-Location
  }
}

$exePath = Join-Path $releaseDir 'meeting-recorder.exe'
if (-not (Test-Path $exePath)) {
  Write-Error "Release binary not found at $exePath. Run with -Build, or build it first: npm run tauri:build:cuda -- --no-bundle"
  exit 1
}

# Every DLL the CUDA release exe needs at load time (see build.rs and
# tauri.cuda.conf.json for how each one lands beside the exe). nvcuda.dll is
# intentionally excluded -- it ships with the NVIDIA display driver, is not
# redistributable, and must already be present on any machine with a
# CUDA-capable driver installed.
$requiredDlls = @(
  'cudart64_13.dll',
  'cublas64_13.dll',
  'cublasLt64_13.dll',
  'onnxruntime.dll',
  'onnxruntime_providers_shared.dll',
  'sherpa-onnx-c-api.dll',
  'sherpa-onnx-cxx-api.dll'
)

foreach ($dll in $requiredDlls) {
  if (-not (Test-Path (Join-Path $releaseDir $dll))) {
    Write-Error "Missing $dll next to $exePath. Rebuild with the cuda feature (npm run tauri:build:cuda, or this script with -Build)."
    exit 1
  }
}

$tauriConf = Get-Content (Join-Path $srcTauri 'tauri.conf.json') -Raw | ConvertFrom-Json
$version = $tauriConf.version

$distRoot = Join-Path $projectRoot 'dist-portable'
$folderName = "MeetingRecorder-$version-cuda-portable"
$outDir = Join-Path $distRoot $folderName

if (Test-Path $outDir) {
  Remove-Item -Recurse -Force $outDir
}
New-Item -ItemType Directory -Path $outDir | Out-Null

Copy-Item $exePath (Join-Path $outDir 'meeting-recorder.exe')
foreach ($dll in $requiredDlls) {
  Copy-Item (Join-Path $releaseDir $dll) (Join-Path $outDir $dll)
}

$readme = @"
Meeting Recorder $version (CUDA, portable) — Windows x64
==========================================================

This folder is a self-contained, GPU-accelerated build of Meeting Recorder.
No installer is required: run meeting-recorder.exe directly from here.

Requirements
------------
- Windows 10 or 11, x64.
- An NVIDIA GPU with a driver that supports CUDA 13
  (nvcuda.dll, part of the driver, is NOT bundled here — install or update
  the driver from https://www.nvidia.com/Download/index.aspx if the app
  reports no compatible GPU; it will fall back to CPU transcription in
  that case rather than fail).
- The Microsoft Edge WebView2 Runtime, which ships with Windows 10/11 by
  default. If it is missing, Windows/Edge will prompt to install the small
  official bootstrapper the first time you run the app.

What is bundled
----------------
meeting-recorder.exe plus every DLL it loads that isn't part of Windows or
the NVIDIA driver:
  cudart64_13.dll, cublas64_13.dll, cublasLt64_13.dll   (NVIDIA CUDA
      redistributables; see NVIDIA's CUDA End User License Agreement,
      redistribution section, for what these DLLs cover)
  onnxruntime.dll, onnxruntime_providers_shared.dll,
  sherpa-onnx-c-api.dll, sherpa-onnx-cxx-api.dll        (sherpa-onnx /
      onnxruntime, used for optional local speaker identification)

Also bundled: a browser-extension\ folder (the optional, local-only Meet/
Teams/Zoom detection helper -- see docs\browser-extension.md). It does
nothing until you use Settings > Browser extension > Connect, then load it
as an unpacked/temporary extension in your browser.

Not truly portable
-------------------
This folder itself is portable (copy it anywhere, run it from a USB drive,
no installation), but the application's own data is NOT: recordings,
transcripts, and settings are still written to
  %APPDATA%\com.local.meetingrecorder
on whichever machine you run it on, the same as the installed build. There
is no per-folder or per-drive data mode.

This build is unsigned. Windows SmartScreen may warn on first run
("Windows protected your PC") -- choose "More info" then "Run anyway" if
you trust the source you obtained this from.
"@

Set-Content -Path (Join-Path $outDir 'README-PORTABLE.txt') -Value $readme -Encoding utf8

# The local-only browser-detection extension (see docs/browser-extension.md)
# and its native-messaging host manifests, so "Open extension folder" in
# Settings and "Connect browser extension" work from a portable install too.
# The private signing key (browser-extension/key/extension-key.pem, if it
# exists locally) and the node-test-only folders are deliberately excluded.
$extensionSrc = Join-Path $projectRoot 'browser-extension'
$extensionDest = Join-Path $outDir 'browser-extension'
New-Item -ItemType Directory -Path $extensionDest | Out-Null
Get-ChildItem -Path $extensionSrc -File | Copy-Item -Destination $extensionDest

$zipPath = Join-Path $distRoot "$folderName.zip"
if (Test-Path $zipPath) {
  Remove-Item -Force $zipPath
}
Compress-Archive -Path $outDir -DestinationPath $zipPath

Write-Output "Portable folder: $outDir"
Write-Output "Portable zip:    $zipPath"
