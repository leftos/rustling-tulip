# Dictation (speech to text) on every client

Speak a prompt into any session from the client you are sitting at, whether that client runs on the machine hosting the daemon or connects to it from another desktop or a phone. The microphone and the recognizer are always on the client; only text crosses the wire.

## Why Claude Code's own voice mode is not enough

Claude Code has built-in dictation (`/voice`, hold Space to talk; [docs](https://code.claude.com/docs/en/voice-dictation.md)). It records from the microphone of the machine running the `claude` process and transcribes on Anthropic's servers, and its docs say it does not work in SSH or cloud sessions.

In rustling-tulip the `claude` process always runs on the daemon's host, so from a remote client `/voice` records the host's microphone, or none. It also only exists in Claude sessions; Codex, Cursor and plain shells have nothing.

This feature leaves `/voice` alone. Locally it still works as before; this gives one dictation path that works the same in every client and every session kind.

## Rulings

- **Engine: the client's own OS recognizer.** Windows: WinRT `Windows.Media.SpeechRecognition.SpeechRecognizer` with the dictation topic constraint, continuous, showing partial results. iOS: `SFSpeechRecognizer`. Android: `android.speech.SpeechRecognizer`. macOS (when the macOS client ships): `SFSpeechRecognizer`. No audio leaves the client for the daemon, and the protocol does not change.
- **Windows engine needs a spike first** (DI.1) before any client code depends on it.
- **Landing: the dictation box, then send.** The transcript appears in a small overlay (the **dictation box**) bound to one session. You edit it there, then:
  - **Enter** inserts the text into the session's prompt as a paste (bracketed when the program asked for it, through `term_input::paste_bytes`, `apps/native/src/term_input.rs:141`) and leaves it unsent, for a last edit in the agent's own input.
  - **Ctrl+Enter** inserts it and then sends Enter, as a separate input write after the paste so the Enter cannot land inside it.
  - **Esc** discards it.
  - **Shift+Enter** adds a newline in the box.
- **Scope: every client, every session kind.** The native client locally and in remote mode, the mobile app, and Claude, Codex, Cursor and plain-shell sessions alike. The text is ordinary PTY input.
- **Desktop key: Right Alt on its own.** A tap (pressed and released with no other key in between) opens the box and starts listening; another tap stops listening. A hold past the tap threshold is push-to-talk: listening runs while the key is held, and releasing it stops listening and leaves the box open for review. Right Alt pressed with another key is AltGr or Alt+key as today and never starts dictation. Space is avoided because Claude Code's `/voice` holds Space inside the PTY.
- **Phone: our own mic button.** A tap or hold mic button on the reply bar (MA6) and on the terminal's key bar (MA8) opens the same review sheet, driven by the platform recognizer with partial results. The phone keyboard's own dictation key still works in the sheet, but the feature does not rely on it.
- **Language: the system speech language.** No setting of ours.
- **Placement:** the desktop part is its own wave now and works locally first. It reaches remote sessions with no extra code once remote mode (Phase 6, RT-34) lands, and the phone part is a mobile-app phase (MA11).

## Design (native client)

### Target and lifecycle

- The box opens on the **focused pane's session** and names it in its header. It stays bound to that session if focus moves elsewhere, so a dictation started on one pane never lands in another.
- If the session ends or is removed while the box is open, the box says so and keeps the text, with a Copy button in place of Insert.
- One box at a time. Opening dictation while a box is open on another session moves the box to the newly focused session and keeps its text, so nothing dictated is lost.
- Listening shows a live mic indicator in the box (level and a "listening" label); partial results draw in a muted colour and become plain text when the recognizer finalizes them.

### Key handling

- GPUI's `Modifiers` does not say which Alt was pressed. The spike (DI.1) settles how to tell Right Alt apart on Windows (the raw key messages GPUI sees, or `GetKeyState(VK_RMENU)` when the modifiers change). It also settles AltGr layouts, where Windows reports Right Alt as Ctrl+Alt with a synthesized left Ctrl.
- The tap/hold threshold is a constant (about 300 ms) chosen in the spike, not a setting.
- A lone Alt release must not open the window's system menu (`WM_SYSKEYUP` → `SC_KEYMENU`); the spike checks GPUI suppresses it.

### Engine

- `apps/native/src/dictation.rs`: a `Recognizer` trait (start, stop, a stream of `Partial(String)` / `Final(String)` / `Error(DictationError)` events), the WinRT implementation, and a scripted fake the UI specs drive. This mirrors `notify.rs` and the planned `speech.rs` (spoken alerts, SA.8).
- Failures are shown in the box with what to do, never swallowed:
  - **Microphone blocked**: "Let desktop apps access your microphone" is off. The box links `ms-settings:privacy-microphone`.
  - **Online speech recognition off**: Windows' dictation topic needs it. The box links `ms-settings:privacy-speech`. The spike confirms whether this is required and whether an offline fallback is usable.
  - **No recognizer for the system language**: the box names the language and links `ms-settings:speech`.
- Privacy: with online recognition, Windows sends the audio to Microsoft, and Apple or Google may do the same on the phone. The first dictation shows this once, in one line, with the platform named.

### Interplay with other features

- **Spoken alerts** (`spoken-alerts.md`, RT-26): alert speech is held while the box is listening, so the recognizer does not transcribe the alert, and plays when listening stops. An open box on a session counts as presence for that session under the alerts' presence rule.
- **Remote mode**: nothing extra. The box sends through the pane's normal input path, which in remote mode goes over the pinned-TLS connection (or the relay) like any keystroke.
- **Claude Code `/voice`**: untouched. Pressing Right Alt in a Claude session never reaches the PTY.

## Design (mobile app)

- A mic button on the MA6 reply bar and the MA8 key bar; tap toggles, hold is push-to-talk, the same rule as the desktop key.
- The review sheet matches the dictation box: target session in its header, partial results, Insert, Insert and send, and Discard.
- Recognizer access through a Flutter plugin wrapping both platform APIs, or a thin platform channel of our own; MA11 chooses and justifies the dependency. iOS needs the `NSSpeechRecognitionUsageDescription` and `NSMicrophoneUsageDescription` strings; Android needs `RECORD_AUDIO`.
- On-device recognition is preferred where the platform offers it (`requiresOnDeviceRecognition` on iOS), falling back to the platform's server recognition with the same one-time privacy line.

## Steps

Item ids are DI.1, DI.2, … (dictation). The parent issue is RT-75.

- [ ] **DI.1 (RT-76) Spike: WinRT dictation in the unpackaged client.** A throwaway binary under `spikes/dictation/` that answers:
  - continuous dictation with partial results from an unpackaged Win32 exe;
  - the microphone consent prompt and the errors when it is denied;
  - whether the "Online speech recognition" setting is required, and the offline result without it;
  - Right Alt detection from GPUI on US and AltGr (Greek, German) layouts, with tap/hold timing and no system-menu activation;
  - accuracy and latency on the same short prompts against Win+H voice typing.

  Findings go in `docs/spikes/dictation.md`.
- [ ] **DI.2 (RT-77) Recognizer.** `apps/native/src/dictation.rs`, `apps/native/Cargo.toml` (`windows` crate features the spike names): the trait, the WinRT recognizer and the scripted fake. Proof: an `#[ignore]`d test that starts and stops the WinRT recognizer and gets a `Final` or a named error, plus unit tests for the event mapping.
- [ ] **DI.3 (RT-78) Dictation box and key.** `apps/native/src/dictation_view.rs`, `term_view.rs` (the input path), `keys.rs` (Right Alt tap/hold), `lib.rs`: the box, target binding, Enter / Ctrl+Enter / Esc / Shift+Enter, session-ended state, error states with their settings links, and the one-time privacy line. Proof: a new `tests/ui/ui_dictation.rs` against the fake recognizer covering:
  - tap and hold;
  - AltGr+key not opening the box;
  - insert as a bracketed and an unbracketed paste;
  - Ctrl+Enter sending the Enter as its own write;
  - focus moving while listening;
  - the session ending with the box open;
  - each error.
- [ ] **DI.4 (RT-80) Alerts interplay.** `apps/native/src/alerts.rs` once SA.9 exists: hold alert speech while listening, and count an open box as presence. Proof: a case in `ui_alerts.rs`. Lands with or after SA.9, whichever is later.
- [ ] **DI.5 (RT-81) Remote check.** With Phase 6 in place, run the native client in remote mode against a daemon on another machine and confirm dictated text reaches the remote session and the host's microphone is never opened. Recorded in the RT-34 verification notes.
- [ ] **DI.6 (RT-82) macOS recognizer.** `SFSpeechRecognizer` behind the same trait, and right Option as the key, with the macOS client work.
- [ ] **MA11 (RT-83) Phone dictation.** See "Design (mobile app)" above and the MA11 line in [mobile-app.md](./mobile-app.md).
- [ ] **DI.7 (RT-79) Docs.** `docs/native-client.md` (the dictation box, the key, the error states); README glossary already has the dictation box.
