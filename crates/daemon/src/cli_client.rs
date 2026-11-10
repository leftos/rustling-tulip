//! The loopback client connection the daemon's own subcommands share: read
//! `daemon.json`, connect only to a listener that proves it holds the token,
//! upgrade to the WebSocket and send the `Hello`.

use anyhow::anyhow;
use daemon_client::{HandshakeFile, ProvenConnectError};
use futures::{SinkExt as _, StreamExt as _};
use protocol::{
    ClientMessage, DaemonHandshake, DaemonMessage, InboundDaemonMessage,
    SUPPORTED_PROTOCOL_VERSIONS,
};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// No daemon runs for this config dir.
#[derive(Debug)]
pub enum NoDaemon {
    /// Its handshake file does not exist.
    NoHandshake(PathBuf),
    /// Its handshake file names a port no connection opens on, the file a
    /// crashed daemon leaves behind; the string says why.
    NothingListening { port: u16, reason: String },
}

impl std::fmt::Display for NoDaemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoHandshake(path) => write!(
                f,
                "no rustling-tulipd is running: {} does not exist",
                path.display()
            ),
            Self::NothingListening { port, reason } => {
                write!(
                    f,
                    "could not reach rustling-tulipd on 127.0.0.1:{port}: {reason}"
                )
            }
        }
    }
}

impl std::error::Error for NoDaemon {}

/// The parts of a [`DaemonMessage::Welcome`] a subcommand acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    pub features: Vec<String>,
}

/// One client connection to the daemon's loopback listener.
pub struct Connection {
    socket: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
}

impl Connection {
    /// Connect to the daemon `handshake_file` names and read up to its
    /// `Welcome`, the listener having `proof` to prove it holds the token.
    /// The initial state follows on the connection. `client_name` is what the
    /// daemon logs the connection under.
    pub async fn open(
        handshake_file: &Path,
        client_name: &str,
        proof: Duration,
    ) -> anyhow::Result<(Self, Welcome)> {
        let mut connection = Self::connect(handshake_file, client_name, proof).await?;
        loop {
            match connection.next_message().await {
                None => {
                    return Err(anyhow!(
                        "rustling-tulipd closed the connection before welcoming it"
                    ));
                }
                Some(DaemonMessage::Welcome { features, .. }) => {
                    return Ok((connection, Welcome { features }));
                }
                Some(DaemonMessage::AuthFailed { reason }) => {
                    return Err(anyhow!("rustling-tulipd refused the handshake: {reason}"));
                }
                Some(_) => {}
            }
        }
    }

    /// Connect as [`Connection::open`] does and send the `Hello`, leaving
    /// every frame of the reply to the caller.
    pub async fn connect(
        handshake_file: &Path,
        client_name: &str,
        proof: Duration,
    ) -> anyhow::Result<Self> {
        let handshake = read_handshake(handshake_file)?;
        let port = handshake.port;
        let stream = match daemon_client::connect_proven(port, &handshake.auth_token, proof).await {
            Ok(stream) => stream,
            Err(ProvenConnectError::NotProven(proof)) => {
                return Err(anyhow!(
                    "{}",
                    crate::cli_pair::not_proven_reason(port, handshake_file, proof)
                ));
            }
            Err(ProvenConnectError::Connect(err)) => {
                return Err(NoDaemon::NothingListening {
                    port,
                    reason: format!("{err:#}"),
                }
                .into());
            }
        };
        let (socket, _) = tokio_tungstenite::client_async(
            format!("ws://127.0.0.1:{port}/ws"),
            MaybeTlsStream::Plain(stream),
        )
        .await
        .map_err(|err| anyhow!("could not reach rustling-tulipd on 127.0.0.1:{port}: {err}"))?;
        let mut connection = Self { socket };
        connection.send(&hello(&handshake, client_name)).await?;
        Ok(connection)
    }

    /// Send one client message.
    pub async fn send(&mut self, message: &ClientMessage) -> anyhow::Result<()> {
        let json = serde_json::to_string(message)
            .map_err(|err| anyhow!("could not serialize a client message: {err}"))?;
        self.socket
            .send(Message::text(json))
            .await
            .map_err(|err| anyhow!("could not send to rustling-tulipd: {err}"))
    }

    /// The next text frame, or `None` once the daemon closed the connection.
    pub async fn next_text(&mut self) -> Option<String> {
        loop {
            match self.socket.next().await {
                None | Some(Err(_) | Ok(Message::Close(_))) => return None,
                Some(Ok(Message::Text(text))) => return Some(text.to_string()),
                Some(Ok(_)) => {}
            }
        }
    }

    /// The next message this build knows, or `None` once the daemon closed
    /// the connection.
    pub async fn next_message(&mut self) -> Option<DaemonMessage> {
        loop {
            let text = self.next_text().await?;
            if let Ok(InboundDaemonMessage::Known(message)) =
                InboundDaemonMessage::from_json_str(&text)
            {
                return Some(*message);
            }
        }
    }

    /// Close the connection. By now the exchange's answer is in hand, so a
    /// failing close changes nothing for the caller.
    pub async fn close(mut self) {
        let _ = self.socket.close(None).await;
    }
}

/// The `daemon.json` the running daemon wrote, or the error that says why
/// there is no daemon to reach.
fn read_handshake(path: &Path) -> anyhow::Result<DaemonHandshake> {
    let bytes = match daemon_client::read_handshake_file(path)? {
        HandshakeFile::Bytes(bytes) => bytes,
        HandshakeFile::Missing(_) => return Err(NoDaemon::NoHandshake(path.to_owned()).into()),
        HandshakeFile::Failed(err) => {
            return Err(anyhow!("could not read {}: {err}", path.display()));
        }
    };
    serde_json::from_slice(&bytes)
        .map_err(|err| anyhow!("could not read {}: {err}", path.display()))
}

/// The `Hello` that opens the connection: the daemon's own token, every
/// protocol version this build speaks, and no client identity.
fn hello(handshake: &DaemonHandshake, client_name: &str) -> ClientMessage {
    ClientMessage::Hello {
        protocol_version: SUPPORTED_PROTOCOL_VERSIONS
            .iter()
            .copied()
            .max()
            .unwrap_or_default(),
        protocol_versions: SUPPORTED_PROTOCOL_VERSIONS.to_vec(),
        auth_token: handshake.auth_token.clone(),
        client_id: None,
        client_name: Some(client_name.to_owned()),
        client_version: None,
        client_kind: None,
        client_features: Vec::new(),
    }
}
