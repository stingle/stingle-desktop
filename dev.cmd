@echo off
rem Build & (re)launch the Stingle Photos desktop app (see dev.ps1).
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0dev.ps1" %*
