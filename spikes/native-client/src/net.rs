//! Daemon connection: reads `daemon.json`, opens the WebSocket, sends `Hello`,
//! then pumps messages both ways on a dedicated tokio thread. GPUI runs its own
//! executor, so the two sides talk over unbounded futures channels.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::{SinkExt as _, StreamExt as _};
use protocol::{ClientMessage, DaemonHandshake, DaemonMessage, InboundDaemonMessage};
use tokio_tungstenite::tungstenite::Message;

pub enum NetEvent {
    Message(Box<DaemonMessage>),
    Closed(String),
}

fn handshake_path() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("RUSTLING_TULIP_CONFIG_DIR") {
        return Ok(PathBuf::from(dir).join("daemon.json"));
    }
    let dirs = directories::ProjectDirs::from("dev", "leftos", "rustling-tulip")
        .context("no home directory to resolve the config dir from")?;
    Ok(dirs.config_dir().join("daemon.json"))
}

fn read_handshake() -> Result<DaemonHandshake> {
    let path = handshake_path()?;
    let raw = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "reading {} — is the daemon running? Start the app once.",
            path.display()
        )
    })?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

/// Spawns the network thread. Every failure ends as a `NetEvent::Closed` with the reason.
pub fn spawn(outbound: UnboundedReceiver<ClientMessage>, inbound: UnboundedSender<NetEvent>) {
    std::thread::spawn(move || {
        let reason = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => match rt.block_on(run(outbound, inbound.clone())) {
                Ok(()) => "daemon closed the connection".to_owned(),
                Err(err) => format!("{err:#}"),
            },
            Err(err) => format!("building tokio runtime: {err}"),
        };
        // The UI may already be gone; nothing else to tell.
        let _ = inbound.unbounded_send(NetEvent::Closed(reason));
    });
}

async fn run(
    mut outbound: UnboundedReceiver<ClientMessage>,
    inbound: UnboundedSender<NetEvent>,
) -> Result<()> {
    let handshake = read_handshake()?;
    let url = format!("ws://127.0.0.1:{}/ws", handshake.port);
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .with_context(|| format!("connecting to {url}"))?;

    let hello = ClientMessage::Hello {
        protocol_version: protocol::PROTOCOL_VERSION,
        protocol_versions: protocol::SUPPORTED_PROTOCOL_VERSIONS.to_vec(),
        auth_token: handshake.auth_token,
        client_id: None,
        client_name: Some("rt-native-spike".to_owned()),
    };
    ws.send(Message::Text(serde_json::to_string(&hello)?.into()))
        .await?;

    loop {
        tokio::select! {
            frame = ws.next() => {
                let Some(frame) = frame else { return Ok(()) };
                let Message::Text(text) = frame.context("reading websocket frame")? else { continue };
                match InboundDaemonMessage::from_json_str(text.as_str()) {
                    Ok(InboundDaemonMessage::Known(msg)) => {
                        if inbound.unbounded_send(NetEvent::Message(msg)).is_err() {
                            return Ok(());
                        }
                    }
                    Ok(InboundDaemonMessage::Unknown { .. }) => {}
                    Err(err) => eprintln!("rt-native-spike: undecodable daemon message: {err}"),
                }
            }
            msg = outbound.next() => {
                let Some(msg) = msg else { return Ok(()) };
                ws.send(Message::Text(serde_json::to_string(&msg)?.into())).await?;
            }
        }
    }
}
