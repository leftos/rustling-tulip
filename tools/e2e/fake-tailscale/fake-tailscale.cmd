@echo off
rem Windows shim for the deterministic tailscale stand-in. Point the daemon at
rem this file via RUSTLING_TULIP_TAILSCALE. Forwards all arguments verbatim.
node "%~dp0index.mjs" %*
