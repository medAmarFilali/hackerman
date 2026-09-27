; Added to Hackerman's installer: see the installer hooks in Tauri's documentation.

; The lock-screen helper is set up outside this app's folder, so that administrator rights are
; needed to change it, and uninstalling the app doesn't reach it. It's left alone by updates,
; where the app is only being replaced.
!macro NSIS_HOOK_PREUNINSTALL
  ${If} $UpdateMode <> 1
    Push $0
    Push $1
    nsExec::ExecToStack 'sc.exe query HackermanLockScreen'
    Pop $0
    Pop $1
    ${If} $0 = 0
      ExecShellWait "runas" "$INSTDIR\${MAINBINARYNAME}.exe" "--remove-lock-screen"
    ${EndIf}
    Pop $1
    Pop $0
  ${EndIf}
!macroend
