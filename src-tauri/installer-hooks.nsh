; T5: NSIS installer hooks for Local API Relay.
;
; The Tauri NSIS template only creates a desktop shortcut from the finish-page
; checkbox (and for silent/passive installs). This hook makes the desktop
; shortcut unconditional so a fresh per-user install always has one.

!macro NSIS_HOOK_POSTINSTALL
  CreateShortcut "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
  !insertmacro SetLnkAppUserModelId "$DESKTOP\${PRODUCTNAME}.lnk"
!macroend
