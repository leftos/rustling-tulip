; rustling-tulip per-user installer (NSIS 3, Modern UI 2).
;
; Installs the three release binaries, a Start menu shortcut, an optional
; desktop shortcut and the optional daemon autostart entry, then registers
; itself in HKCU's uninstall list. No elevation: everything lives under
; $LOCALAPPDATA and HKCU.
;
; Built by `.\rt.ps1 installer`, which passes VERSION, NUMVERSION, BINDIR and
; OUTFILE; see tools/installer/README.md.

Unicode true
SetCompressor /SOLID lzma
RequestExecutionLevel user

!include "MUI2.nsh"

!ifndef VERSION
  !error "VERSION is not defined: pass /DVERSION=<x.y.z[-suffix]>."
!endif
!ifndef NUMVERSION
  !error "NUMVERSION is not defined: pass /DNUMVERSION=<x.y.z>."
!endif
!ifndef BINDIR
  !error "BINDIR is not defined: pass /DBINDIR=<path to target\release>."
!endif
!ifndef OUTFILE
  !error "OUTFILE is not defined: pass /DOUTFILE=<path to the setup exe>."
!endif

!define APPEXE "rustling-tulip-native.exe"
!define DAEMONEXE "rustling-tulipd.exe"
!define TRACEREXE "rt-tracer.exe"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\rustling-tulip"
!define RUNKEY "Software\Microsoft\Windows\CurrentVersion\Run"
!define RUNVALUE "rustling-tulip-daemon"

Name "rustling-tulip"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\rustling-tulip"
InstallDirRegKey HKCU "${UNINSTKEY}" "InstallLocation"

VIProductVersion "${NUMVERSION}.0"
VIAddVersionKey "ProductName" "rustling-tulip"
VIAddVersionKey "FileVersion" "${NUMVERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "CompanyName" "leftos"
VIAddVersionKey "FileDescription" "rustling-tulip installer"
; NSIS warns (9100, an error under /WX) when the standard key is missing.
VIAddVersionKey "LegalCopyright" "leftos. MIT License."

!define MUI_ICON "${__FILEDIR__}\icon.ico"
!define MUI_UNICON "${__FILEDIR__}\icon.ico"
!define MUI_FINISHPAGE_RUN "$INSTDIR\${APPEXE}"
!define MUI_FINISHPAGE_RUN_TEXT "Run rustling-tulip"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; Stops a running daemon gracefully before the binaries are copied or deleted.
;
; The daemon and tracer run from the content-addressed binary cache, so the
; files installed here are not locked by a live session's tracer. The daemon
; process itself must go so the freshly-installed one can take its port and
; write daemon.json. POSTing /shutdown leaves every rt-tracer.exe alive, so
; the next daemon reattaches and the user's sessions resurface unchanged.
; Failures are swallowed: a missing or unhealthy daemon already meets the
; goal of "nothing holds the installed files".
!macro RT_GRACEFUL_SHUTDOWN
  Push $0
  Push $1
  Push $R0

  StrCpy $R0 "$APPDATA\leftos\rustling-tulip\config\daemon.json"
  IfFileExists "$R0" 0 rt_shutdown_done

  ; Pass the daemon.json path via env var so PowerShell sees it as a literal
  ; string -- avoids quote-in-quote gymnastics inside the -Command payload.
  System::Call 'kernel32::SetEnvironmentVariableA(t "RT_DAEMON_JSON", t "$R0")'

  ; POST to /shutdown with the bearer token. -TimeoutSec 5 so a hung daemon
  ; doesn't block install indefinitely. Every `$` of the PowerShell payload is
  ; doubled: NSIS would otherwise read `$h` as one of its own variables, and
  ; /WX turns that warning into an error. `$$` reaches PowerShell as `$`.
  nsExec::ExecToLog 'powershell -NoProfile -ExecutionPolicy Bypass -Command "try { $$h = Get-Content -Raw $$env:RT_DAEMON_JSON | ConvertFrom-Json; Invoke-RestMethod -Method Post -TimeoutSec 5 -Uri (\"http://127.0.0.1:$$($$h.port)/shutdown\") -Headers @{ Authorization = \"Bearer $$($$h.auth_token)\" } } catch { }"'
  Pop $0

  ; Poll for daemon.json removal -- proof the daemon shut down cleanly. Up to
  ; 5 seconds in 250ms slices; after that proceed regardless, and a still
  ; running daemon surfaces as a sharing violation the user can resolve.
  StrCpy $1 0
rt_poll:
  IfFileExists "$R0" 0 rt_shutdown_done
  IntOp $1 $1 + 1
  IntCmp $1 20 rt_shutdown_done
  Sleep 250
  Goto rt_poll

rt_shutdown_done:
  Pop $R0
  Pop $1
  Pop $0
!macroend

Section "rustling-tulip" SEC_APP
  SectionIn RO
  SetOutPath "$INSTDIR"
  !insertmacro RT_GRACEFUL_SHUTDOWN

  File "${BINDIR}\${APPEXE}"
  File "${BINDIR}\${DAEMONEXE}"
  File "${BINDIR}\${TRACEREXE}"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateShortcut "$SMPROGRAMS\rustling-tulip.lnk" "$INSTDIR\${APPEXE}"

  WriteRegStr HKCU "${UNINSTKEY}" "DisplayName" "rustling-tulip"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTKEY}" "Publisher" "leftos"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayIcon" "$INSTDIR\${APPEXE}"
  WriteRegStr HKCU "${UNINSTKEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINSTKEY}" "InstallLocation" "$INSTDIR"
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoRepair" 1
SectionEnd

Section "Start the daemon when I sign in" SEC_AUTOSTART
  WriteRegStr HKCU "${RUNKEY}" "${RUNVALUE}" '"$INSTDIR\${DAEMONEXE}" --detach'
SectionEnd

Section /o "Desktop shortcut" SEC_DESKTOP
  CreateShortcut "$DESKTOP\rustling-tulip.lnk" "$INSTDIR\${APPEXE}"
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_APP} "The rustling-tulip client, the daemon it talks to and the tracer that keeps terminal sessions alive."
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_AUTOSTART} "Start the daemon in the background when you sign in, so the client connects at once."
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_DESKTOP} "Put a rustling-tulip shortcut on the desktop."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Section "Uninstall"
  !insertmacro RT_GRACEFUL_SHUTDOWN

  DeleteRegValue HKCU "${RUNKEY}" "${RUNVALUE}"

  Delete "$INSTDIR\${APPEXE}"
  Delete "$INSTDIR\${DAEMONEXE}"
  Delete "$INSTDIR\${TRACEREXE}"
  Delete "$INSTDIR\uninstall.exe"

  Delete "$SMPROGRAMS\rustling-tulip.lnk"
  Delete "$DESKTOP\rustling-tulip.lnk"

  DeleteRegKey HKCU "${UNINSTKEY}"
  RMDir "$INSTDIR"
SectionEnd
