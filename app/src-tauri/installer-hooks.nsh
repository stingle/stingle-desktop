; NSIS installer hooks for Stingle Desktop.
;
; Installs the bundled WinFsp driver silently during our (already-elevated)
; install, and removes it on uninstall — so the virtual drive works with no
; separate prompt. Enable by pointing tauri.conf.json at this file:
;
;   "bundle": {
;     "windows": {
;       "nsis": { "installerHooks": "installer-hooks.nsh" }
;     }
;   }
;
; Requires resources/winfsp.msi to be bundled (see resources/README.md). If you
; don't bundle the MSI, do NOT enable this hook — NSIS will fail to find it.

!macro NSIS_HOOK_POSTINSTALL
  ; The bundled MSI is unpacked into $INSTDIR\resources\ (matching the
  ; tauri.conf "resources" mapping).
  DetailPrint "Installing WinFsp (virtual drive driver)..."
  ; /passive shows a progress bar; /qn would be fully silent. We are already
  ; elevated inside the installer, so this needs no extra UAC prompt.
  ExecWait 'msiexec /i "$INSTDIR\resources\winfsp.msi" /passive /norestart' $0
  DetailPrint "WinFsp installer exit code: $0"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; Best-effort silent removal of WinFsp on uninstall. Leave commented out if
  ; other software on the machine might rely on WinFsp.
  ; ExecWait 'msiexec /x "$INSTDIR\resources\winfsp.msi" /qn /norestart' $0
!macroend
