<#
  Build & (re)launch the Stingle Desktop app in dev mode.

  Usage:
    Double-click  dev.cmd
    or in a terminal:   .\dev.cmd      (cmd or PowerShell)
    or directly:        powershell -ExecutionPolicy Bypass -File .\dev.ps1

  It stops any running instance / dev server, then runs `npm run tauri dev`
  which compiles the Rust backend + frontend and opens the app. Closing the
  app window stops the script.

  Release installer instead:   cd app ; npm run tauri build
#>
$ErrorActionPreference = "SilentlyContinue"

# Ensure Rust/cargo are reachable even if not yet on the user PATH.
$cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
if (Test-Path $cargoBin) { $env:Path = "$cargoBin;$env:Path" }

# The Windows virtual-drive adapter (`vfs-winfsp`) links WinFsp, whose -sys crate
# runs bindgen and needs libclang. Point at the scoop-installed LLVM if the env
# var isn't already set, so the build works even from a shell that predates it.
if (-not $env:LIBCLANG_PATH) {
    $llvmBin = Join-Path $env:USERPROFILE "scoop\apps\llvm\current\bin"
    if (Test-Path (Join-Path $llvmBin "libclang.dll")) { $env:LIBCLANG_PATH = $llvmBin }
}

Write-Host "==> Stopping any running Stingle app / dev server..." -ForegroundColor Cyan
Get-Process app -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
# Free the Vite dev port (1420) if something is still holding it.
$conn = Get-NetTCPConnection -LocalPort 1420 -State Listen -ErrorAction SilentlyContinue
if ($conn) { $conn.OwningProcess | ForEach-Object { Stop-Process -Id $_ -Force -ErrorAction SilentlyContinue } }
Start-Sleep -Seconds 1

$appDir = Join-Path $PSScriptRoot "app"
if (-not (Test-Path (Join-Path $appDir "node_modules"))) {
    Write-Host "==> Installing frontend dependencies (first run)..." -ForegroundColor Cyan
    Push-Location $appDir; npm install; Pop-Location
}

Write-Host "==> Building and launching (npm run tauri dev, vfs-winfsp on)..." -ForegroundColor Cyan
Set-Location $appDir
# `vfs-winfsp` builds the read-only virtual-drive adapter into the app so the
# Settings > Virtual drive toggle is functional. Requires WinFsp installed.
npm run tauri dev -- --features vfs-winfsp
