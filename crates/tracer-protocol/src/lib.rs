//! Stable wire protocol between `rustling-tulipd` and `rt-tracer.exe`.
//!
//! This ABI is **deliberately small** because every change here forces a
//! re-spawn of every live session — the in-process tracers can't be hot-
//! upgraded. The main protocol (`crates/protocol`) carries everything else
//! (git operations, presets, tabs, config) so the tracer pipe stays free of
//! that churn.
//!
//! Versioning mirrors the main protocol's range-based handshake
//! (`SUPPORTED_TRACER_VERSIONS`), but tracer bumps are reserved for
//! genuinely breaking changes to the I/O / lifecycle surface — additive
//! changes ride on `#[serde(default)]` and `Unknown` fallback.
//!
//! Stability contract: see `docs/tracer-abi.md` for the long-form description
//! of what counts as a bump and how rollouts are staged.

use serde::{Deserialize, Serialize};

/// Current ABI version. Bumped only for removed messages, renamed fields,
/// or semantic changes — additive changes (new variant, new
/// `#[serde(default)]` field) ride on the wrapper machinery.
pub const TRACER_VERSION: u32 = 1;

/// Every version this build of the daemon/tracer can speak. Sorted high → low
/// so negotiation picks the newest mutually-supported.
pub const SUPPORTED_TRACER_VERSIONS: &[u32] = &[TRACER_VERSION];

/// First frame the daemon sends after connecting to a tracer pipe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracerHello {
    /// Highest version the daemon is willing to speak.
    pub version: u32,
    /// Range advertised so the tracer can downgrade if it predates `version`.
    #[serde(default)]
    pub supported: Vec<u32>,
}

/// Tracer's response to `TracerHello` — confirms the negotiated version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracerWelcome {
    /// Negotiated version: max common of daemon's range and tracer's.
    pub version: u32,
    #[serde(default)]
    pub supported: Vec<u32>,
}

/// Daemon → tracer messages. Keep the surface tiny: only what's needed to
/// drive a single PTY-attached child. Git, config, and orchestration stay
/// in the main protocol where they belong.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TracerRequest {
    /// Bytes to feed into the child's PTY stdin. Base64-encoded so the JSON
    /// envelope stays text.
    Input { data_b64: String },
    /// Resize the child's PTY. Mirrors `xterm`'s `WindowSize`.
    Resize { cols: u16, rows: u16 },
    /// One-shot health probe. The tracer replies with `TracerResponse::Status`.
    Status,
    /// Politely shut down. The tracer kills its child, drains the ring to
    /// disk, and exits. The pipe closes after the response.
    Stop,
}

/// Tracer → daemon messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TracerResponse {
    /// PTY output chunk. The tracer drains its ring on `(re)connect` so the
    /// daemon doesn't miss bytes emitted while the connection was down.
    Output { data_b64: String },
    /// Snapshot reply to `TracerRequest::Status`.
    Status {
        child_pid: Option<u32>,
        child_alive: bool,
        /// Approximate bytes in the ring buffer at the moment of the snapshot.
        /// Useful for the daemon to size its initial replay.
        ring_bytes: usize,
    },
    /// Child process exited. Sent once; the tracer then drains the ring and
    /// follows up with its own exit.
    Exited { code: i32 },
    /// Asynchronous error not tied to a specific request.
    Error { message: String },
}

/// Parse-time wrapper around [`TracerRequest`]; mirrors `InboundClientMessage`
/// in the main protocol. Unknown request types are captured here instead of
/// crashing the tracer's parse loop — important because the tracer may be
/// older than the daemon talking to it.
#[derive(Debug, Clone)]
pub enum InboundTracerRequest {
    Known(TracerRequest),
    Unknown {
        type_tag: String,
        raw: serde_json::Value,
    },
}

impl InboundTracerRequest {
    pub fn from_json_str(s: &str) -> Result<Self, serde_json::Error> {
        let value: serde_json::Value = serde_json::from_str(s)?;
        if let Ok(known) = serde_json::from_value::<TracerRequest>(value.clone()) {
            return Ok(Self::Known(known));
        }
        let type_tag = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| "<missing-type>".to_string(), String::from);
        Ok(Self::Unknown {
            type_tag,
            raw: value,
        })
    }
}

/// Symmetric wrapper for daemon-side consumption of tracer responses.
#[derive(Debug, Clone)]
pub enum InboundTracerResponse {
    Known(TracerResponse),
    Unknown {
        type_tag: String,
        raw: serde_json::Value,
    },
}

impl InboundTracerResponse {
    pub fn from_json_str(s: &str) -> Result<Self, serde_json::Error> {
        let value: serde_json::Value = serde_json::from_str(s)?;
        if let Ok(known) = serde_json::from_value::<TracerResponse>(value.clone()) {
            return Ok(Self::Known(known));
        }
        let type_tag = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| "<missing-type>".to_string(), String::from);
        Ok(Self::Unknown {
            type_tag,
            raw: value,
        })
    }
}

/// Pipe name format. The daemon picks the session id; the tracer derives the
/// pipe name from it via this function so both sides agree without an
/// explicit handshake step.
#[must_use]
pub fn pipe_name(session_id: &str) -> String {
    format!(r"\\.\pipe\rt-tracer-{session_id}")
}

/// Negotiate the highest mutually supported ABI version.
/// Returns `None` when the intersection of advertised versions is empty.
#[must_use]
pub fn negotiate(scalar: u32, range: &[u32]) -> Option<u32> {
    let mut best: Option<u32> = None;
    for &v in std::iter::once(&scalar).chain(range.iter()) {
        if SUPPORTED_TRACER_VERSIONS.contains(&v) {
            best = Some(best.map_or(v, |cur| cur.max(v)));
        }
    }
    best
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert preconditions with expect/panic"
)]
mod tests {
    use super::*;

    #[test]
    fn input_request_round_trips() {
        let req = TracerRequest::Input {
            data_b64: "aGk=".to_string(),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let decoded: TracerRequest = serde_json::from_str(&json).expect("decode");
        assert!(matches!(decoded, TracerRequest::Input { data_b64 } if data_b64 == "aGk="));
    }

    #[test]
    fn inbound_request_unknown_captures_payload() {
        let json = r#"{"type":"future_only","field":1}"#;
        let parsed = InboundTracerRequest::from_json_str(json).expect("parse");
        let InboundTracerRequest::Unknown { type_tag, .. } = parsed else {
            panic!("expected Unknown");
        };
        assert_eq!(type_tag, "future_only");
    }

    #[test]
    fn negotiate_picks_supported() {
        assert_eq!(negotiate(TRACER_VERSION, &[TRACER_VERSION]), Some(TRACER_VERSION));
        assert_eq!(negotiate(u32::MAX, &[]), None);
    }

    #[test]
    fn pipe_name_format_is_session_id_suffixed() {
        let pipe = pipe_name("abc-123");
        assert_eq!(pipe, r"\\.\pipe\rt-tracer-abc-123");
    }
}
