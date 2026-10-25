@echo off
rem Windows shim for the deterministic claude stand-in. Point the daemon at
rem this file via RUSTLING_TULIP_CLAUDE so it spawns the Node script under a
rem ConPTY-friendly host. Forwards all flags verbatim.
node "%~dp0index.mjs" %*
