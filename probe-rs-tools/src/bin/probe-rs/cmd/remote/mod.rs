use std::path::PathBuf;

#[cfg(feature = "remote")]
use std::path::Path;

use probe_rs::{
    flashing::{BootInfo, ProgressEvent},
    probe::{list::Lister, WireProtocol},
    Session,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    cmd::remote::functions::{
        attach::{Attach, AttachResult},
        chip::{ChipData, ChipFamily},
        flash::{DownloadOptions, Flash, FlashResult},
        info::{Info, InfoEvent},
        list_probes::{DebugProbeEntry, ListProbes},
        monitor::{Monitor, MonitorEvent, MonitorMode, MonitorOptions},
        read_memory::ReadMemory,
        reset::ResetCore,
        resume::ResumeAllCores,
        test::{ListTests, RunTest, Test, TestResult, Tests},
        write_memory::WriteMemory,
        Context, NoMessage, RemoteFunction, RemoteFunctions, Word,
    },
    util::common_options::ProbeOptions,
    FormatOptions,
};

#[cfg(feature = "remote")]
pub mod client;
pub mod functions;
#[cfg(feature = "remote")]
pub mod server;

#[derive(Serialize, Deserialize)]
enum ClientMessage {
    TempFile(Vec<u8>),
    Rpc(RemoteFunctions),
}

#[derive(Debug, Serialize, Deserialize)]
enum ServerMessage {
    TempFileOpened(PathBuf),
    RpcResult(Vec<u8>),
    RpcMessage(Vec<u8>),
    Error(String),
}

/// The client handle used to execute remote functions.
trait Client: Sized + Send + 'static {
    async fn run_call<F: RemoteFunction<Message = NoMessage>>(
        &mut self,
        func: F,
    ) -> anyhow::Result<F::Result> {
        self.run_call_streaming(func, |_| {}).await
    }

    async fn run_call_streaming<F, CB>(&mut self, func: F, on_msg: CB) -> anyhow::Result<F::Result>
    where
        F: RemoteFunction,
        CB: FnMut(F::Message) + Send;
}

#[allow(
    private_bounds,
    reason = "The supertrait is a private implementation detail, code should rely on this API"
)]
pub trait ClientInterface: Client {
    async fn attach_probe(
        &mut self,
        probe_options: ProbeOptions,
        resume_target: bool,
    ) -> anyhow::Result<AttachResult> {
        self.run_call(Attach {
            probe_options,
            resume_target,
        })
        .await
    }

    async fn list_probes(&mut self) -> anyhow::Result<Vec<DebugProbeEntry>> {
        self.run_call(ListProbes::new()).await
    }

    async fn info(
        &mut self,
        probe_options: &ProbeOptions,
        target_sel: Option<u32>,
        protocol: WireProtocol,
        on_msg: impl FnMut(InfoEvent) + Send,
    ) -> anyhow::Result<()> {
        self.run_call_streaming(
            Info {
                probe_options: probe_options.clone(),
                target_sel,
                protocol,
            },
            on_msg,
        )
        .await
    }

    async fn load_chip_families(
        &mut self,
        families: Vec<probe_rs_target::ChipFamily>,
    ) -> anyhow::Result<()> {
        self.run_call(functions::chip::LoadChipFamilies { families })
            .await
    }

    async fn list_chip_families(&mut self) -> anyhow::Result<Vec<ChipFamily>> {
        self.run_call(functions::chip::ListFamilies::new()).await
    }

    async fn chip_info(&mut self, name: &str) -> anyhow::Result<ChipData> {
        self.run_call(functions::chip::ChipInfo::new(name.to_string()))
            .await
    }
}

pub struct SessionInterface<'a, T: ClientInterface> {
    sessid: SessionId,
    iface: &'a mut T,
}

impl<'a, T: ClientInterface> SessionInterface<'a, T> {
    pub fn new(iface: &'a mut T, sessid: SessionId) -> Self {
        Self { sessid, iface }
    }

    pub fn into_session_id(self) -> SessionId {
        self.sessid
    }

    pub async fn resume_all_cores(&mut self) -> anyhow::Result<()> {
        self.iface
            .run_call(ResumeAllCores {
                sessid: self.sessid,
            })
            .await
    }

    pub fn core(&mut self, core: usize) -> CoreInterface<'_, T> {
        CoreInterface {
            sessid: self.sessid,
            core,
            iface: self.iface,
        }
    }

    pub async fn flash(
        &mut self,
        path: PathBuf,
        format: FormatOptions,
        options: DownloadOptions,
        on_msg: impl FnMut(ProgressEvent) + Send,
    ) -> anyhow::Result<FlashResult> {
        self.iface
            .run_call_streaming(
                Flash {
                    sessid: self.sessid,
                    path,
                    format,
                    options,
                },
                on_msg,
            )
            .await
    }

    pub async fn monitor(
        &mut self,
        mode: MonitorMode,
        path: PathBuf,
        options: MonitorOptions,
        on_msg: impl FnMut(MonitorEvent) + Send,
    ) -> anyhow::Result<()> {
        self.iface
            .run_call_streaming(
                Monitor {
                    sessid: self.sessid,
                    mode,
                    path,
                    options,
                },
                on_msg,
            )
            .await
    }

    pub async fn list_tests(&mut self, boot_info: BootInfo) -> anyhow::Result<Tests> {
        self.iface
            .run_call(ListTests {
                sessid: self.sessid,
                boot_info,
            })
            .await
    }

    pub async fn run_test(&mut self, test: Test) -> anyhow::Result<TestResult> {
        self.iface
            .run_call(RunTest {
                sessid: self.sessid,
                test,
            })
            .await
    }
}

pub struct CoreInterface<'a, T: ClientInterface> {
    sessid: SessionId,
    core: usize,
    iface: &'a mut T,
}

impl<T: ClientInterface> CoreInterface<'_, T> {
    #[allow(private_bounds)]
    pub async fn read_memory<W>(&mut self, address: u64, count: usize) -> anyhow::Result<Vec<W>>
    where
        W: Word + DeserializeOwned,
        ReadMemory<W>: RemoteFunction<Message = NoMessage, Result = Vec<W>>,
    {
        self.iface
            .run_call(ReadMemory::<W> {
                core: self.core,
                sessid: self.sessid,
                address,
                count,
                _phantom: Default::default(),
            })
            .await
    }

    #[allow(private_bounds)]
    pub async fn write_memory<W>(&mut self, address: u64, data: Vec<W>) -> anyhow::Result<()>
    where
        W: Word + Serialize,
        WriteMemory<W>: RemoteFunction<Message = NoMessage, Result = ()>,
    {
        self.iface
            .run_call(WriteMemory::<W> {
                core: self.core,
                sessid: self.sessid,
                address,
                data,
            })
            .await
    }

    pub async fn reset(&mut self) -> anyhow::Result<()> {
        self.iface
            .run_call(ResetCore {
                core: self.core,
                sessid: self.sessid,
            })
            .await
    }
}

impl<T> ClientInterface for T where T: Client {}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct SessionId(());

/// Run functions locally.
pub struct LocalSession {
    session: Option<Session>,
    dry_run: bool,
}

impl LocalSession {
    pub fn new() -> Self {
        Self {
            session: None,
            dry_run: false,
        }
    }

    pub fn set_session(&mut self, session: Session, dry_run: bool) -> SessionId {
        self.session = Some(session);
        self.dry_run = dry_run;
        SessionId(())
    }

    pub fn session(&mut self, _sid: SessionId) -> &mut Session {
        self.session.as_mut().unwrap()
    }

    pub fn dry_run(&self, _sid: SessionId) -> bool {
        self.dry_run
    }

    pub fn lister(&self) -> Lister {
        Lister::new()
    }
}

impl Client for LocalSession {
    async fn run_call_streaming<F, CB>(
        &mut self,
        func: F,
        mut on_msg: CB,
    ) -> anyhow::Result<F::Result>
    where
        F: RemoteFunction,
        CB: FnMut(F::Message) + Send,
    {
        let ctx = Context::new(self, move |msg: Vec<u8>| {
            match postcard::from_bytes(&msg) {
                Ok(msg) => on_msg(msg),
                Err(err) => tracing::error!("Failed to parse message: {err}"),
            };
        });

        func.run(ctx).await
    }
}

/// Run functions on the remote server.
#[cfg(feature = "remote")]
pub struct RemoteSession {
    client: client::ClientConnection,
}

#[cfg(feature = "remote")]
impl RemoteSession {
    pub fn new(client: client::ClientConnection) -> Self {
        Self { client }
    }

    pub async fn upload_file(&mut self, path: impl AsRef<Path>) -> anyhow::Result<PathBuf> {
        self.client.upload_file(path.as_ref()).await
    }
}

#[cfg(feature = "remote")]
impl Client for RemoteSession {
    async fn run_call_streaming<F, CB>(
        &mut self,
        mut func: F,
        mut on_msg: CB,
    ) -> anyhow::Result<F::Result>
    where
        F: RemoteFunction,
        CB: FnMut(F::Message) + Send,
    {
        func.prepare_remote(self).await?;

        self.client
            .run_call(func, move |msg| {
                match postcard::from_bytes(&msg) {
                    Ok(msg) => on_msg(msg),
                    Err(err) => tracing::error!("Failed to parse message: {err}"),
                };
            })
            .await
    }
}

/// Runs the blocking closure on a separate thread, with option for emitting events to the client.
pub async fn run_blocking_streaming<R, M, F>(
    ctx: Context<'_, impl functions::EmitterFn>,
    f: F,
) -> anyhow::Result<R>
where
    M: Serialize + Send + 'static,
    R: Send + 'static,
    F: FnOnce(&mut LocalSession, UnboundedSender<M>) -> anyhow::Result<R> + Send + 'static,
{
    let (iface, mut emitter) = ctx.split();
    let mut moved_iface = std::mem::replace(iface, LocalSession::new());

    // Create channel for events.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<M>();

    // Run the blocking function on a separate thread. Note that we can't stop this thread.
    let worker = tokio::task::spawn_blocking({
        move || {
            let rv = f(&mut moved_iface, tx);
            (moved_iface, rv)
        }
    });

    // Catch events and emit them to the client.
    while let Some(event) = rx.recv().await {
        emitter.emit(event).await?;
    }

    let (moved_iface, rv) = worker.await?;

    *iface = moved_iface;

    rv
}
