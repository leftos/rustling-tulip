import { PROTOCOL_VERSION, SUPPORTED_PROTOCOL_VERSIONS, type ClientMessage } from "./protocol";

/**
 * The first frame the connection sends. `protocol_versions` advertises every
 * version this page can speak, and `auth_token` is the stored device token
 * (`""` when the page has none, which the daemon answers with `Welcome`
 * followed by `DevicePairFailed`). `client_kind` tells the host it is the
 * phone app, which carries no build version. No `client_id`/`client_name`:
 * the daemon falls back to the shared legacy layout for a client that sends
 * neither.
 */
export function buildHello(token: string | undefined): ClientMessage {
  return {
    type: "hello",
    protocol_version: PROTOCOL_VERSION,
    protocol_versions: [...SUPPORTED_PROTOCOL_VERSIONS],
    auth_token: token ?? "",
    client_kind: "phone",
  };
}
