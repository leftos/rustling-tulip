import { createContext, useContext } from "react";

/// Whether the current window is connected to a remote host (vs the bundled
/// local daemon). Provided at the App root; consumed by components that expose
/// local-filesystem actions, which target the *client* machine and are
/// therefore meaningless when the daemon runs elsewhere.
///
/// Defaults to `false` (local) so any component rendered outside the provider
/// — there should be none in practice — degrades to the safe, full-capability
/// path rather than hiding actions unexpectedly.
export const RemoteModeContext = createContext(false);

export function useIsRemote(): boolean {
  return useContext(RemoteModeContext);
}

/// Window event dispatched when a user triggers a local-FS action that's
/// unavailable on a remote connection. App listens for it and shows a toast,
/// so deeply-nested components (terminal links, context menus) don't need the
/// toast dispatcher threaded in.
export const REMOTE_UNAVAILABLE_EVENT = "rt:remote-unavailable";

export interface RemoteUnavailableDetail {
  action: string;
}

/// Tooltip shown on disabled local-FS affordances in remote mode.
export const REMOTE_UNAVAILABLE_TOOLTIP =
  "Unavailable on remote connections — this would act on your machine, not the host.";

/// Surface the "unavailable on remote connections" toast for a named action.
/// Use from click handlers that can't be cleanly disabled (e.g. terminal file
/// links); persistent buttons should disable + show REMOTE_UNAVAILABLE_TOOLTIP.
export function notifyRemoteUnavailable(action: string): void {
  window.dispatchEvent(
    new CustomEvent<RemoteUnavailableDetail>(REMOTE_UNAVAILABLE_EVENT, {
      detail: { action },
    }),
  );
}
