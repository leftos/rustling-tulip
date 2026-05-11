import { invoke } from "@tauri-apps/api/core";

/// Writes one line to `%APPDATA%\leftos\rustling-tulip\config\logs\app.log`
/// via the `log_message` Tauri command. Fire-and-forget — failures fall back
/// to the dev console so dropping a log line never crashes the app. Useful
/// for paths (shutdown, window-close, daemon-handshake) where the user can
/// hand me the file after the fact instead of relying on a live console.
export function logToFile(
  level: "info" | "warn" | "error",
  message: string,
): void {
  void invoke("log_message", { level, message }).catch((err: unknown) => {
    // Don't recurse via logToFile here; the file path itself is the suspect.
    console.warn("log_message invoke failed", err);
  });
}
