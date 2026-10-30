@echo off
rem Windows shim for the deterministic ssh stand-in. Point the native client at
rem this file via RUSTLING_TULIP_SSH. Forwards all arguments verbatim.
node "%~dp0index.mjs" %*
