; rustling-tulip per-user installer (NSIS 3, Modern UI 2).
;
; Installs the three release binaries, a Start menu shortcut, an optional
; desktop shortcut and the optional daemon autostart entry, puts its folder on
; the user PATH, then registers itself in HKCU's uninstall list. No elevation:
; everything lives under $LOCALAPPDATA and HKCU.
;
; Built by `.\rt.ps1 installer`, which passes VERSION, NUMVERSION, BINDIR and
; OUTFILE; see tools/installer/README.md.

Unicode true
SetCompressor /SOLID lzma
RequestExecutionLevel user

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"

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
; The client's toast activator class, which the client registers itself at
; startup (with its Start-menu shortcut): ACTIVATOR_CLSID in
; apps/native/src/toast_activation.rs.
!define TOASTCLSIDKEY "Software\Classes\CLSID\{354BE9DC-6D82-4269-8883-D13CE3E38648}"

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

; Stops a running daemon before the uninstaller deletes the files, with the
; installed daemon's own `shutdown`: it sends the token only to a listener
; that proves it holds it, and leaves every rt-tracer.exe alive. The running
; daemon is a cached copy, so the installed exe is never locked. Its exit
; code is only logged: a missing or unproven daemon must not fail the
; uninstall. An install stops the daemon through `apply-update` instead.
!macro RT_STOP_DAEMON
  Push $0
  IfFileExists "$INSTDIR\${DAEMONEXE}" 0 rt_stop_done
  nsExec::ExecToLog '"$INSTDIR\${DAEMONEXE}" shutdown'
  Pop $0
  DetailPrint "rustling-tulipd shutdown exited with $0"
rt_stop_done:
  Pop $0
!macroend

; Adds the install folder to, or removes it from, the user PATH
; (HKCU\Environment, value Path) with the installed user-path.ps1, because an
; SSH client runs `rustling-tulipd bridge` on this host by name and Windows
; sshd starts it with the user's PATH from the registry. PowerShell, not NSIS
; strings, edits the value: an NSIS string holds 1024 characters, and a user
; PATH can be longer. The script prints its own outcome, which ExecToLog puts
; in the details; a failure is logged and the setup carries on.
;
; A folder ending in `\` (a drive root such as D:\) gets that backslash
; doubled: the command-line parser reads `\"` as a literal quote, so "D:\"
; would arrive as D:", while "D:\\" arrives as D:\.
; user-path.tests.ps1 mirrors this quoting; keep the two in step.
!macro RT_USER_PATH ACTION DIR
  Push $0
  Push $1
  StrCpy $1 "${DIR}"
  StrCpy $0 $1 "" -1
  ${If} $0 == "\"
    StrCpy $1 "$1\"
  ${EndIf}
  nsExec::ExecToLog '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$INSTDIR\user-path.ps1" -${ACTION} -Dir "$1"'
  Pop $0
  ${If} $0 != 0
    DetailPrint "The user PATH was not changed: user-path.ps1 -${ACTION} exited with $0."
  ${EndIf}
  Pop $1
  Pop $0
!macroend

; Drops one trailing `\` from VAR, so "X" and "X\" compare equal. Uses $2.
!macro RT_TRIM_TRAILING_BACKSLASH VAR
  StrCpy $2 ${VAR} "" -1
  ${If} $2 == "\"
    StrCpy ${VAR} ${VAR} -1
  ${EndIf}
!macroend

; The install folder of the previous install, read before this one rewrites it.
Var PreviousInstDir
; The previous install's DisplayVersion, empty on a first install.
Var OldVersion
; 1 when the command line holds /UPDATE: the files are applied, and of the
; registry only DisplayVersion is written, once they are in place.
Var UpdateMode
; 1 when the command line holds /RELAUNCH: `apply-update` starts the client
; again when it ends, whatever its outcome.
Var RelaunchMode

Function .onInit
  ${GetParameters} $R0
  ClearErrors
  ${GetOptions} $R0 "/UPDATE" $R1
  ${IfNot} ${Errors}
    StrCpy $UpdateMode 1
  ${EndIf}
  ClearErrors
  ${GetOptions} $R0 "/RELAUNCH" $R1
  ${IfNot} ${Errors}
    StrCpy $RelaunchMode 1
  ${EndIf}
  ClearErrors
FunctionEnd

; Puts the files staged in $INSTDIR\.staging in place with the staged daemon's
; own `apply-update` (crates/daemon/src/cli_apply_update.rs): it takes the
; update mutex, waits for the client to exit, stops the running daemon before
; writing any file, refuses while anything still runs from $INSTDIR, swaps
; each file in with the client last, rolls every swap back on a failure,
; writes last-result.json and, under /RELAUNCH, starts the client again.
; Leaves its exit code in $0 ("error" when it could not be started) and its
; output in $1, then removes the staging folder.
;
; A --to folder ending in `\` (a drive root) gets that backslash doubled, as
; in RT_USER_PATH, so it never forms `\"` with the closing quote.
Function RtApplyUpdate
  StrCpy $3 $INSTDIR
  StrCpy $2 $3 "" -1
  ${If} $2 == "\"
    StrCpy $3 "$3\"
  ${EndIf}
  StrCpy $2 '"$INSTDIR\.staging\${DAEMONEXE}" apply-update --from "$INSTDIR\.staging" --to "$3"'
  ${If} $OldVersion != ""
    StrCpy $2 '$2 --old-version "$OldVersion"'
  ${EndIf}
  ${If} $UpdateMode == 1
    StrCpy $2 "$2 --update"
  ${EndIf}
  ${If} $RelaunchMode == 1
    StrCpy $2 "$2 --relaunch"
  ${EndIf}
  nsExec::ExecToStack $2
  Pop $0
  Pop $1
  DetailPrint "rustling-tulipd apply-update exited with $0: $1"
  RMDir /r "$INSTDIR\.staging"
FunctionEnd

; A reinstall pointed at another folder takes the previous folder off the user
; PATH, so sshd never runs the stale exe left there and uninstalling the new
; folder leaves no entry behind.
Function RtRemovePreviousInstDirFromPath
  Push $0
  Push $1
  Push $2
  StrCpy $0 $PreviousInstDir
  StrCpy $1 $INSTDIR
  !insertmacro RT_TRIM_TRAILING_BACKSLASH $0
  !insertmacro RT_TRIM_TRAILING_BACKSLASH $1
  ${If} $PreviousInstDir != ""
  ${AndIf} $0 != $1
    !insertmacro RT_USER_PATH Remove $PreviousInstDir
  ${EndIf}
  Pop $2
  Pop $1
  Pop $0
FunctionEnd

Section "rustling-tulip" SEC_APP
  SectionIn RO
  ClearErrors
  ReadRegStr $PreviousInstDir HKCU "${UNINSTKEY}" "InstallLocation"
  ClearErrors
  ReadRegStr $OldVersion HKCU "${UNINSTKEY}" "DisplayVersion"
  ClearErrors
  ; Every file goes to the staging folder; only `apply-update` writes $INSTDIR.
  RMDir /r "$INSTDIR\.staging"
  SetOutPath "$INSTDIR\.staging"
  File "${BINDIR}\${DAEMONEXE}"
  File "${BINDIR}\${TRACEREXE}"
  File "${BINDIR}\${APPEXE}"
  File "${__FILEDIR__}\user-path.ps1"
  WriteUninstaller "$INSTDIR\.staging\uninstall.exe"
  SetOutPath "$INSTDIR"
  Call RtApplyUpdate
  ${If} $0 != 0
    ${If} $1 == ""
      StrCpy $1 "Setup could not update the app (code $0)."
    ${Else}
      StrCpy $1 "Setup could not update the app (code $0):$\r$\n$\r$\n$1"
    ${EndIf}
    ${If} $0 == "error"
      StrCpy $0 1
    ${EndIf}
    MessageBox MB_OK|MB_ICONSTOP $1 /SD IDOK
    SetErrorLevel $0
    Abort
  ${EndIf}

  ; An update leaves the Run value, the shortcuts, the PATH and the rest of
  ; the uninstall key as the user's install set them.
  ${If} $UpdateMode == 1
    WriteRegStr HKCU "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
    Return
  ${EndIf}

  CreateShortcut "$SMPROGRAMS\rustling-tulip.lnk" "$INSTDIR\${APPEXE}"

  WriteRegStr HKCU "${UNINSTKEY}" "DisplayName" "rustling-tulip"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTKEY}" "Publisher" "leftos"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayIcon" "$INSTDIR\${APPEXE}"
  WriteRegStr HKCU "${UNINSTKEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINSTKEY}" "InstallLocation" "$INSTDIR"
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoRepair" 1

  Call RtRemovePreviousInstDirFromPath
  !insertmacro RT_USER_PATH Add $INSTDIR
SectionEnd

Section "Start the daemon when I sign in" SEC_AUTOSTART
  ${If} $UpdateMode != 1
    WriteRegStr HKCU "${RUNKEY}" "${RUNVALUE}" '"$INSTDIR\${DAEMONEXE}" --detach'
  ${EndIf}
SectionEnd

Section /o "Desktop shortcut" SEC_DESKTOP
  ${If} $UpdateMode != 1
    CreateShortcut "$DESKTOP\rustling-tulip.lnk" "$INSTDIR\${APPEXE}"
  ${EndIf}
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_APP} "The rustling-tulip client, the daemon it talks to and the tracer that keeps terminal sessions alive."
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_AUTOSTART} "Start the daemon in the background when you sign in, so the client connects at once."
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_DESKTOP} "Put a rustling-tulip shortcut on the desktop."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Section "Uninstall"
  !insertmacro RT_STOP_DAEMON

  DeleteRegValue HKCU "${RUNKEY}" "${RUNVALUE}"
  DeleteRegKey HKCU "${TOASTCLSIDKEY}"
  !insertmacro RT_USER_PATH Remove $INSTDIR

  Delete "$INSTDIR\${APPEXE}"
  Delete "$INSTDIR\${DAEMONEXE}"
  Delete "$INSTDIR\${TRACEREXE}"
  Delete "$INSTDIR\user-path.ps1"
  Delete "$INSTDIR\uninstall.exe"
  Delete "$INSTDIR\*.old"
  Delete "$INSTDIR\*.new"
  RMDir /r "$INSTDIR\.staging"

  Delete "$SMPROGRAMS\rustling-tulip.lnk"
  Delete "$DESKTOP\rustling-tulip.lnk"

  DeleteRegKey HKCU "${UNINSTKEY}"
  RMDir "$INSTDIR"
SectionEnd
