//! Remote client
//!
//! The client opens a websocket connection to the host, sends a token to authenticate and
//! then sends commands to the server. The commands are handled by the server (by the same
//! handlers that are used for the local commands) and the output is streamed back to the client.
//!
//! The command output may be a result and/or a stream of messages encoded as [ServerMessage].

use anyhow::Context as _;
use axum::http::Uri;
use futures_util::{SinkExt, StreamExt as _};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{ClientRequestBuilder, Message},
    MaybeTlsStream, WebSocketStream,
};

use std::{
    path::{Path, PathBuf},
    str::FromStr,
};

use super::{ClientMessage, ServerMessage};
use crate::cmd::remote::functions::RemoteFunction;

/// Represents a connection to a remote server.
///
/// Internally implemented as a websocket connection.
pub struct ClientConnection {
    websocket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    is_localhost: bool,
}

impl ClientConnection {
    pub async fn upload_file(&mut self, path: impl AsRef<Path>) -> anyhow::Result<PathBuf> {
        let path = path.as_ref();
        if self.is_localhost {
            return Ok(path.to_path_buf());
        }

        let data = tokio::fs::read(path).await.context("Failed to read file")?;
        let msg = ClientMessage::TempFile(data);
        let msg = postcard::to_stdvec(&msg).context("Failed to serialize client message")?;
        self.websocket.send(Message::Binary(msg.into())).await?;

        if let Some(Ok(Message::Binary(msg))) = self.websocket.next().await {
            let msg = postcard::from_bytes::<ServerMessage>(&msg)
                .context("Failed to parse server message")?;
            match msg {
                ServerMessage::TempFileOpened(path) => return Ok(path),
                msg => panic!("Command unexpectedly returned {msg:?}"),
            }
        }

        anyhow::bail!("Server did not return a file path")
    }

    pub(super) async fn run_call<F, CB>(
        &mut self,
        f: F,
        mut on_message: CB,
    ) -> anyhow::Result<F::Result>
    where
        F: RemoteFunction,
        CB: FnMut(Vec<u8>),
    {
        let msg = ClientMessage::Rpc(f.into());
        let msg = postcard::to_stdvec(&msg).context("Failed to serialize client message")?;

        self.websocket.send(Message::Binary(msg.into())).await?;

        while let Some(Ok(msg)) = self.websocket.next().await {
            match msg {
                Message::Binary(msg) => {
                    let msg = postcard::from_bytes::<ServerMessage>(&msg)
                        .context("Failed to parse server message")?;
                    match msg {
                        ServerMessage::RpcResult(msg) => {
                            let msg = postcard::from_bytes::<F::Result>(&msg)
                                .context("Failed to parse RPC response")?;
                            return Ok(msg);
                        }
                        ServerMessage::RpcMessage(msg) => on_message(msg),
                        ServerMessage::Error(msg) => anyhow::bail!("{msg}"),
                        msg => panic!("Command unexpectedly returned {msg:?}"),
                    }
                }
                Message::Close(_) => {}
                msg => panic!("Server unexpectedly sent {msg:?}"),
            }
        }

        anyhow::bail!("Connection closed unexpectedly")
    }
}

pub async fn connect(host: &str, token: Option<String>) -> anyhow::Result<ClientConnection> {
    let uri = Uri::from_str(&format!("{}/worker", host)).context("Failed to parse server URI")?;

    let is_localhost = uri
        .host()
        .is_some_and(|h| ["localhost", "127.0.0.1", "::1"].contains(&h));
    let req = ClientRequestBuilder::new(uri).with_header(
        "User-Agent",
        format!("probe-rs-tools {}", env!("PROBE_RS_LONG_VERSION")),
    );

    let req = if let Some(token) = token {
        req.with_header("Authorization", format!("Bearer {token}"))
    } else {
        req
    };

    let (ws_stream, _) = connect_async(req).await.context("Failed to connect")?;

    Ok(ClientConnection {
        websocket: ws_stream,
        is_localhost,
    })
}
