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

        let msg = ClientMessage::TempFile(data);
        let msg = postcard::to_stdvec(&msg).context("Failed to serialize client message")?;
        self.websocket.send(Message::Binary(msg.into())).await?;

        let Some(Ok(Message::Binary(msg))) = self.websocket.next().await else {
            anyhow::bail!("Server did not return a file path");
        };

        let msg = postcard::from_bytes::<ServerMessage>(&msg)
            .context("Failed to parse server message")?;
        let ServerMessage::TempFileOpened(path) = msg else {
            anyhow::bail!("Command unexpectedly returned {msg:?}");
        };

        self.uploaded_files.insert(src_path, path.clone());

        Ok(path)
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
        struct WebsocketGuard<'a>(&'a mut WebSocketStream<MaybeTlsStream<TcpStream>>);

        impl WebsocketGuard<'_> {
            fn defuse(self) {
                std::mem::forget(self);
            }
        }

        impl Drop for WebsocketGuard<'_> {
            fn drop(&mut self) {
                async_scoped::TokioScope::scope_and_block(|s| {
                    s.spawn(async {
                        let msg = postcard::to_stdvec(&ClientMessage::CancelRpc)
                            .expect("Failed to serialize client message");
                        self.0.send(Message::Binary(msg.into())).await?;

                        // We'll still have to wait for the server to acknowledge the cancel
                        while let Some(Ok(msg)) = self.0.next().await {
                            match msg {
                                Message::Binary(msg) => {
                                    let msg = postcard::from_bytes::<ServerMessage>(&msg)
                                        .expect("Failed to parse server message");
                                    match msg {
                                        ServerMessage::RpcResult(_) => return Ok(()),
                                        ServerMessage::RpcMessage(_) => {}
                                        ServerMessage::Error(msg) => anyhow::bail!("{msg}"),
                                        msg => panic!("Command unexpectedly returned {msg:?}"),
                                    }
                                }
                                Message::Close(_) => {}
                                msg => panic!("Server unexpectedly sent {msg:?}"),
                            }
                        }

                        Ok(())
                    })
                });
            }
        }

        let websocket_guard = WebsocketGuard(&mut self.websocket);

        let msg = ClientMessage::Rpc(f.into());
        let msg = postcard::to_stdvec(&msg).context("Failed to serialize client message")?;

        websocket_guard.0.send(Message::Binary(msg.into())).await?;

        while let Some(Ok(msg)) = websocket_guard.0.next().await {
            match msg {
                Message::Binary(msg) => {
                    let msg = postcard::from_bytes::<ServerMessage>(&msg)
                        .context("Failed to parse server message")?;
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
        uploaded_files: HashMap::new(),
    })
}
