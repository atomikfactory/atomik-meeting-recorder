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

Push-Location $projectRoot
try { npm run tauri dev } finally { Pop-Location }
