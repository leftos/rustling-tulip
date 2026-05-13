; Pre-install / pre-uninstall hooks for rustling-tulip.
;
; Why these exist:
;
;   The default Tauri NSIS template only checks whether the GUI app exe
;   (rustling-tulip.exe) is running. It does not touch rustling-tulipd.exe
;   or rt-tracer.exe. On Windows those .exe files cannot be overwritten
;   while a process is using them, so an upgrade or reinstall against a
;   running daemon would silently fail mid-copy.
;
;   The fix is twofold:
;
;     1. The daemon and tracer are spawned from a content-addressed binary
;        cache (see crates/daemon/src/binary_cache.rs and
;        apps/tauri-app/src-tauri/src/daemon_supervisor.rs::cache_daemon_binary).
;        The shipped templates never get exec'd directly, so they're never
;        locked even when sessions are running. Reinstall replaces the
;        templates in place.
;
;     2. We still need the running daemon process to die so the new daemon
;        can take its port + write daemon.json. This hook sends a graceful
;        Shutdown over the daemon's HTTP /shutdown endpoint, which leaves
;        every rt-tracer.exe supervisor alive — the freshly-installed
;        daemon will reattach to them on next launch and the user's PTY
;        sessions resurface unchanged.

!include "WordFunc.nsh"

!macro RT_GRACEFUL_SHUTDOWN
  Push $0
  Push $1
  Push $R0

  StrCpy $R0 "$APPDATA\leftos\rustling-tulip\config\daemon.json"
  IfFileExists "$R0" 0 rt_shutdown_done

  ; Pass the daemon.json path via env var so PowerShell sees it as a literal
  ; string — avoids quote-in-quote gymnastics inside the -Command payload.
  System::Call 'kernel32::SetEnvironmentVariableA(t "RT_DAEMON_JSON", t "$R0")'

  ; POST to /shutdown with the bearer token. -TimeoutSec 5 so a hung daemon
  ; doesn't block install indefinitely; failures are swallowed (try/catch)
  ; because a missing/unhealthy daemon is fine — the goal is "no daemon
  ; locking files when we hit File copy" and a dead daemon already meets
  ; that bar.
  nsExec::ExecToLog 'powershell -NoProfile -ExecutionPolicy Bypass -Command "try { $h = Get-Content -Raw $env:RT_DAEMON_JSON | ConvertFrom-Json; Invoke-RestMethod -Method Post -TimeoutSec 5 -Uri (\"http://127.0.0.1:$($h.port)/shutdown\") -Headers @{ Authorization = \"Bearer $($h.auth_token)\" } } catch { }"'
  Pop $0

  ; Poll for daemon.json removal — proof the daemon shut down cleanly. Up
  ; to 5 seconds in 250ms slices. After that we proceed regardless;
  ; binary-cache means a still-locked tracer template wouldn't break us,
  ; and a still-running daemon means the install will surface a sharing
  ; violation that the user can resolve manually.
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

!macro NSIS_HOOK_PREINSTALL
  !insertmacro RT_GRACEFUL_SHUTDOWN
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro RT_GRACEFUL_SHUTDOWN
!macroend
