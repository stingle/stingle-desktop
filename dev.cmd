@echo off
rem Build & (re)launch the Stingle Desktop app (see dev.ps1).
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0dev.ps1" %*
