#!/usr/bin/env node
/**
 * Deterministic stand-in for the `tailscale` CLI the daemon's tailnet listener
 * runs (`crates/tailnet/src/cli.rs`). `RUSTLING_TULIP_TAILSCALE` points the
 * daemon at it (via the `fake-tailscale.cmd` wrapper on Windows), and
 * `RUSTLING_TULIP_FAKE_TAILSCALE_STATE` names the JSON state file it answers
 * from:
 *
 *   {
 *     "user_id": <the owner's numeric Tailscale user id>,
 *     "login": "<the owner's login, for display>",          (optional)
 *     "whois_user_id": <the peer's user id>,                 (optional; default user_id)
 *     "cert_file": "<PEM certificate for localhost>",
 *     "key_file": "<PEM key for that certificate>"
 *   }
 *
 * Commands:
 *   - `whoami --json`: the host node `localhost.`, the owner's, untagged, at
 *     `127.0.0.1/32`, so the listener binds loopback.
 *   - `whois --json <ip:port>`: the peer node, `whois_user_id`'s (the owner's
 *     by default), another node id, untagged, at `100.101.102.103/32`: the
 *     address a daemon built with `e2e-loopback-peers` judges a loopback peer
 *     as (`PEER_TAILNET_IP` in `crates/daemon/src/tailnet_listener.rs`).
 *   - `status --json`: this node alone, online, with no peers.
 *   - `cert --cert-file F --key-file K [--min-validity Ns] -- <name>`: copies
 *     the state's `cert_file` and `key_file` to F and K.
 *
 * Anything else, a missing state file or a failed copy exits 1 with a line on
 * stderr, which the daemon reports as a failed CLI call.
 */
import { copyFileSync, readFileSync } from "node:fs";

/// Prefix of every line this shim prints on stderr.
const TAG = "[fake-tailscale]";
const STATE_ENV = "RUSTLING_TULIP_FAKE_TAILSCALE_STATE";
/// The host's MagicDNS name: `localhost` resolves to loopback with no DNS or
/// hosts-file entry, and the certificate is issued for it.
const HOST_NAME = "localhost.";
const HOST_ADDRESS = "127.0.0.1";
const PEER_ADDRESS = "100.101.102.103";
const DEFAULT_LOGIN = "owner@example.com";

const argv = process.argv.slice(2);
const state = loadState();
const [command, ...rest] = argv;

switch (command) {
  case "whoami":
    requireJsonFlag(rest);
    print(whoami());
    break;
  case "whois":
    requireJsonFlag(rest);
    if (rest.length !== 2) {
      fail(`whois takes --json and one address, got: ${rest.join(" ")}`);
    }
    print(whois());
    break;
  case "status":
    requireJsonFlag(rest);
    print(status());
    break;
  case "cert":
    cert(rest);
    break;
  default:
    fail(`unknown command: ${argv.join(" ")}`);
}

/**
 * @param {string} message
 * @returns {never}
 */
function fail(message) {
  process.stderr.write(`${TAG} ${message}\n`);
  process.exit(1);
}

/** The state file `STATE_ENV` names, checked for the fields every answer needs. */
function loadState() {
  const path = process.env[STATE_ENV];
  if (!path) {
    fail(`${STATE_ENV} is not set; it names the state file this fake answers from`);
  }
  let parsed;
  try {
    parsed = JSON.parse(readFileSync(path, "utf8"));
  } catch (err) {
    fail(`reading the state file ${path}: ${err?.message ?? String(err)}`);
  }
  for (const field of ["user_id", "cert_file", "key_file"]) {
    if (parsed[field] === undefined) {
      fail(`the state file ${path} has no ${field}`);
    }
  }
  return parsed;
}

/** @param {string[]} args */
function requireJsonFlag(args) {
  if (args[0] !== "--json") {
    fail(`expected --json, got: ${args.join(" ")}`);
  }
}

/** @param {object} value */
function print(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}

/** @param {number} userId */
function userProfile(userId) {
  return {
    ID: userId,
    LoginName: state.login ?? DEFAULT_LOGIN,
    DisplayName: "E2E Owner",
  };
}

function whoami() {
  return {
    Node: {
      ID: 1,
      StableID: "nHostE2E",
      Name: HOST_NAME,
      User: state.user_id,
      Addresses: [`${HOST_ADDRESS}/32`],
      Tags: [],
    },
    UserProfile: userProfile(state.user_id),
  };
}

function whois() {
  const user = state.whois_user_id ?? state.user_id;
  return {
    Node: {
      ID: 2,
      StableID: "nPeerE2E",
      Name: "e2e-peer.",
      User: user,
      Addresses: [`${PEER_ADDRESS}/32`],
      Tags: [],
    },
    UserProfile: userProfile(user),
  };
}

function status() {
  return {
    BackendState: "Running",
    Self: {
      ID: "nHostE2E",
      HostName: "localhost",
      DNSName: HOST_NAME,
      TailscaleIPs: [HOST_ADDRESS],
      UserID: state.user_id,
      Online: true,
    },
    Peer: {},
  };
}

/**
 * `cert --cert-file F --key-file K [--min-validity Ns] -- <name>`.
 *
 * @param {string[]} args
 */
function cert(args) {
  let certFile = null;
  let keyFile = null;
  let name = null;
  for (let i = 0; i < args.length; i++) {
    const arg = args[i];
    if (arg === "--cert-file" && args[i + 1] !== undefined) {
      certFile = args[++i];
    } else if (arg === "--key-file" && args[i + 1] !== undefined) {
      keyFile = args[++i];
    } else if (arg === "--min-validity" && args[i + 1] !== undefined) {
      i++;
    } else if (arg === "--" && args.length === i + 2) {
      name = args[++i];
    } else {
      fail(`cert: unexpected argument ${arg}`);
    }
  }
  if (certFile === null || keyFile === null || name === null) {
    fail(`cert needs --cert-file, --key-file and -- <name>, got: ${args.join(" ")}`);
  }
  try {
    copyFileSync(state.cert_file, certFile);
    copyFileSync(state.key_file, keyFile);
  } catch (err) {
    fail(`cert: copying the certificate for ${name}: ${err?.message ?? String(err)}`);
  }
}
