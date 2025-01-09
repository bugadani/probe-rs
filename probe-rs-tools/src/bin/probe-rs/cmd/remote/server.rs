//! Remote server
//!
//! The server listens for incoming websocket connections and executes commands on behalf of the
//! client. The server also provides a status webpage that shows the available probes.
//!
//! The commands are executed in separate processes and the output is streamed back to the client.
//! The server tracks opened probes and ensures that only one command is executed per probe at a time.

use anyhow::Context as _;
use axum::{
    extract::{
        ws::{self, WebSocket},
        State, WebSocketUpgrade,
    },
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use axum_extra::{
    headers::{self, authorization},
    TypedHeader,
};
use futures_util::{stream::SplitSink, SinkExt as _, StreamExt as _};
use probe_rs::probe::list::Lister;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use std::{fmt::Write, io::Write as _, sync::Arc};

use super::{ClientMessage, ServerMessage};
use crate::{
    cmd::remote::{
        functions::{Context, EmitterFn},
        LocalSession,
    },
    Config,
};

struct ServerState {
    config: Config,
}

impl ServerState {
    fn new(config: Config) -> Self {
        Self { config }
    }
}

async fn server_info() -> Html<String> {
    let mut body = String::new();
    body.push_str("<!DOCTYPE html>");
    body.push_str("<html>");
    body.push_str("<head>");
    body.push_str("<title>probe-rs server info</title>");
    body.push_str("</head>");
    body.push_str("<body>");
    body.push_str("<h1>probe-rs status</h1>");

    let probes = Lister::new().list_all();
    if probes.is_empty() {
        body.push_str("<p>No probes connected</p>");
    } else {
        body.push_str("<ul>");
        for probe in probes {
            write!(body, "<li>{}</li>", probe).unwrap();
        }
    }

    body.push_str("</ul>");

    write!(body, "<p>Version: {}</p>", env!("PROBE_RS_LONG_VERSION")).unwrap();

    body.push_str("</body>");
    body.push_str("</html>");

    Html(body)
}

#[derive(clap::Parser, Serialize, Deserialize)]
pub struct Cmd {}

impl Cmd {
    pub async fn run(self, config: Config) -> anyhow::Result<()> {
        if config.server_users.is_empty() {
            tracing::warn!("No users configured.");
        }

        let state = Arc::new(ServerState::new(config));

        let app = Router::new()
            .route("/", get(server_info))
            .route("/worker", get(ws_handler))
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();

        tracing::info!("listening on {}", listener.local_addr().unwrap());

        axum::serve(listener, app).await?;

        Ok(())
    }
}

pub struct ServerConnection {
    websocket: SplitSink<WebSocket, ws::Message>,
    temp_files: Vec<NamedTempFile>,
}

impl ServerConnection {
    async fn send_message(&mut self, msg: ServerMessage) -> anyhow::Result<()> {
        let msg = postcard::to_stdvec(&msg).context("Failed to serialize message")?;
        self.websocket.send(ws::Message::Binary(msg)).await?;

        Ok(())
    }

    async fn save_temp_file(&mut self, data: Vec<u8>) -> anyhow::Result<()> {
        let mut file = NamedTempFile::new().context("Failed to write temporary file")?;

        file.as_file_mut()
            .write_all(&data)
            .context("Failed to write temporary file")?;

        let path = file.path().to_path_buf();
        tracing::info!("Saved temporary file to {}", path.display());
        self.temp_files.push(file);

        let msg = ServerMessage::TempFileOpened(path);
        self.send_message(msg)
            .await
            .context("Failed to send file path")
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    state: State<Arc<ServerState>>,
    TypedHeader(auth): TypedHeader<headers::Authorization<authorization::Bearer>>,
) -> impl IntoResponse {
    // TODO: version check based on user agent
    let token = auth.0.token();
    let Some(user) = state.config.server_users.iter().find(|u| token == u.token) else {
        tracing::info!("Unknown token: {}", token);
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    };

    tracing::info!("User {} connected", user.name);

    // finalize the upgrade process by returning upgrade callback.
    // we can customize the callback by sending additional info such as address.
    ws.on_upgrade(move |socket| handle_socket(socket, state.0))
}

/// Actual websocket statemachine (one will be spawned per connection)
async fn handle_socket(socket: WebSocket, _state: Arc<ServerState>) {
    let (writer, mut reader) = socket.split();
    let mut handle = ServerConnection {
        websocket: writer,
        temp_files: vec![],
    };
    let mut session = LocalSession::new();

    while let Some(Ok(msg)) = reader.next().await {
        if let ws::Message::Binary(msg) = msg {
            let msg =
                postcard::from_bytes::<ClientMessage>(&msg).expect("Failed to deserialize message");
            match msg {
                ClientMessage::TempFile(data) => handle.save_temp_file(data).await.unwrap(),
                ClientMessage::Rpc(function) => {
                    struct RpcEmitter<'a>(&'a mut ServerConnection);
                    impl EmitterFn for RpcEmitter<'_> {
                        async fn call(&mut self, msg: Vec<u8>) -> anyhow::Result<()> {
                            let msg = ServerMessage::RpcMessage(msg);
                            self.0.send_message(msg).await
                        }
                    }

                    let ctx = Context::new(&mut session, RpcEmitter(&mut handle));

                    let response = match function.run(ctx).await {
                        Ok(r) => ServerMessage::RpcResult(r),
                        Err(e) => ServerMessage::Error(format!("{:?}", e)),
                    };
                    handle.send_message(response).await.unwrap();
                }
            }
        }
    }
}
