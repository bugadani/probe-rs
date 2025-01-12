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
    collections::HashMap,
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
    uploaded_files: HashMap<PathBuf, PathBuf>,
}

impl ClientConnection {
    pub async fn upload_file(&mut self, src_path: &Path) -> anyhow::Result<PathBuf> {
        let src_path = src_path.canonicalize()?;
        if self.is_localhost {
            return Ok(src_path);
        }

        if let Some(path) = self.uploaded_files.get(&src_path) {
            return Ok(path.clone());
        }

        let data = tokio::fs::read(&src_path)
            .await
            .context("Failed to read file")?;
        tracing::debug!("Uploading {} ({} bytes)", src_path.display(), data.len());

        self.send_message(ClientMessage::TempFile(data)).await?;

        let Some(Ok(ServerMessage::TempFileOpened(path))) = self.read_server_message().await else {
            anyhow::bail!("Server did not return a file path");
        };

        tracing::debug!("Uploaded file to {}", path.display());
        self.uploaded_files.insert(src_path, path.clone());

        Ok(path)
    }

    async fn send_message(&mut self, msg: ClientMessage) -> anyhow::Result<()> {
        let msg = postcard::to_stdvec(&msg).context("Failed to serialize message")?;

        // Send message length first
        self.websocket
            .send(Message::Binary(
                (msg.len() as u64).to_le_bytes().to_vec().into(),
            ))
            .await
            .context("Failed to send message length")?;

        // Send message
        self.websocket
            .send(Message::Binary(msg.into()))
            .await
            .context("Failed to send message")
    }

    async fn read_message(&mut self) -> Option<anyhow::Result<Message>> {
        // Read the length of the message
        let mut data = Vec::with_capacity(128);

        while data.len() < 8 {
            match self.websocket.next().await {
                Some(Ok(Message::Binary(msg))) => data.extend_from_slice(&msg),
                Some(Ok(Message::Close(_))) => return None,
                Some(Ok(other)) if data.is_empty() => return Some(Ok(other)),
                Some(Ok(other)) => {
                    return Some(Err(anyhow::anyhow!("Unexpected message: {:?}", other)));
                }
                Some(Err(err)) => return Some(Err(err.into())),
                None => return None,
            };
        }

        let len = u64::from_le_bytes(data[0..8].try_into().unwrap());
        data.drain(..8);

        // Read the message
        while data.len() < len as usize {
            match self.websocket.next().await {
                Some(Ok(Message::Binary(msg))) => data.extend_from_slice(&msg),
                Some(Ok(Message::Close(_))) => return None,
                Some(Ok(other)) => {
                    return Some(Err(anyhow::anyhow!("Unexpected message: {:?}", other)));
                }
                Some(Err(err)) => return Some(Err(err.into())),
                None => return None,
            };
        }

        Some(Ok(Message::Binary(data.into())))
    }

    async fn read_server_message(&mut self) -> Option<anyhow::Result<ServerMessage>> {
        match self.read_message().await? {
            Ok(Message::Binary(msg)) => {
                tracing::debug!("Received message: {} bytes", msg.len());
                Some(
                    postcard::from_bytes::<ServerMessage>(&msg)
                        .context("Failed to parse client message"),
                )
            }
            Ok(Message::Close(_)) => None,
            msg => Some(Err(anyhow::anyhow!("Server unexpectedly sent {msg:?}"))),
        }
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
        struct WebsocketGuard<'a>(&'a mut ClientConnection);

        impl WebsocketGuard<'_> {
            fn defuse(self) {
                std::mem::forget(self);
            }
        }

        impl Drop for WebsocketGuard<'_> {
            fn drop(&mut self) {
                async_scoped::TokioScope::scope_and_block(|s| {
                    s.spawn(async {
                        self.0.send_message(ClientMessage::CancelRpc).await?;

                        // We'll still have to wait for the server to acknowledge the cancel
                        while let Some(Ok(msg)) = self.0.read_server_message().await {
                            match msg {
                                ServerMessage::RpcResult(_) => return Ok(()),
                                ServerMessage::RpcMessage(_) => {}
                                ServerMessage::Error(msg) => anyhow::bail!("{msg}"),
                                msg => panic!("Command unexpectedly returned {msg:?}"),
                            }
                        }

                        Ok(())
                    })
                });
            }
        }

        let websocket_guard = WebsocketGuard(self);

        websocket_guard
            .0
            .send_message(ClientMessage::Rpc(f.into()))
            .await?;

        while let Some(Ok(msg)) = websocket_guard.0.read_server_message().await {
            match msg {
                ServerMessage::RpcResult(msg) => {
                    // We don't want to cancel the RPC anymore (in case deserialization fails)
                    websocket_guard.defuse();

                    tracing::debug!("Received RPC result: {} bytes", msg.len());

                    let msg = postcard::from_bytes::<F::Result>(&msg)
                        .context("Failed to parse RPC response")?;
                    return Ok(msg);
                }
                ServerMessage::RpcMessage(msg) => on_message(msg),
                ServerMessage::Error(msg) => {
                    // We don't need to cancel the RPC if the server returns an error
                    websocket_guard.defuse();
                    anyhow::bail!("{msg}")
                }
                msg => panic!("Command unexpectedly returned {msg:?}"),
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
        uploaded_files: HashMap::new(),
    })
}
