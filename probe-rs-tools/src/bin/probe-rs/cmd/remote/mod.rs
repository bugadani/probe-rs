use std::{
    any::Any,
    collections::HashMap,
    future::Future,
    marker::PhantomData,
    ops::DerefMut,
    path::PathBuf,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

#[cfg(feature = "remote")]
use std::path::Path;

use probe_rs::{
    flashing::{BootInfo, ProgressEvent},
    probe::{list::Lister, WireProtocol},
    Session,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::{
    runtime::Handle,
    sync::{mpsc::UnboundedSender, Mutex},
};
use tokio_util::sync::CancellationToken;

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
        stack_trace::{StackTraces, TakeStackTrace},
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
    CancelRpc,
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
    sessid: Key<Session>,
    iface: &'a mut T,
}

impl<'a, T: ClientInterface> SessionInterface<'a, T> {
    pub fn new(iface: &'a mut T, sessid: Key<Session>) -> Self {
        Self { sessid, iface }
    }

    pub fn into_session_id(self) -> Key<Session> {
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

    pub async fn stack_trace(&mut self, path: PathBuf) -> anyhow::Result<StackTraces> {
        self.iface
            .run_call(TakeStackTrace {
                sessid: self.sessid,
                path,
            })
            .await
    }
}

pub struct CoreInterface<'a, T: ClientInterface> {
    sessid: Key<Session>,
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

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Hash)]
pub struct Key<T> {
    key: u64,
    marker: PhantomData<T>,
}

impl<T> Clone for Key<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Key<T> {}

impl<T> Key<T> {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        Self {
            key: COUNTER.fetch_add(1, Ordering::Relaxed),
            marker: PhantomData,
        }
    }
}

/// Run functions locally.
pub struct LocalSession {
    dry_run: bool,
    object_storage: HashMap<u64, Arc<Mutex<dyn Any + Send>>>,
}

impl LocalSession {
    pub fn new() -> Self {
        Self {
            dry_run: false,
            object_storage: HashMap::new(),
        }
    }

    pub fn store_object<T: Any + Send>(&mut self, obj: T) -> Key<T> {
        let key = Key::new();
        self.object_storage
            .insert(key.key, Arc::new(Mutex::new(obj)));
        key
    }

    pub async fn object_mut<T: Any + Send>(&self, key: Key<T>) -> impl DerefMut<Target = T> + Send {
        let obj = self.object_storage.get(&key.key).unwrap();
        let guard = obj.clone().lock_owned().await;
        tokio::sync::OwnedMutexGuard::map(guard, |e: &mut (dyn Any + Send)| {
            e.downcast_mut::<T>().unwrap()
        })
    }

    pub fn object_mut_blocking<T: Any + Send>(
        &self,
        key: Key<T>,
    ) -> impl DerefMut<Target = T> + Send {
        let obj = self.object_storage.get(&key.key).unwrap();
        let guard = obj.clone().blocking_lock_owned();
        tokio::sync::OwnedMutexGuard::map(guard, |e: &mut (dyn Any + Send)| {
            e.downcast_mut::<T>().unwrap()
        })
    }

    pub fn set_session(&mut self, session: Session, dry_run: bool) -> Key<Session> {
        let key = self.store_object(session);
        self.dry_run = dry_run;
        key
    }

    pub fn session_blocking(&self, sid: Key<Session>) -> impl DerefMut<Target = Session> + Send {
        self.object_mut_blocking(sid)
    }

    pub fn dry_run(&self, _sid: Key<Session>) -> bool {
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
        // F's implementation may be blocking. To allow it to complete in a semi-graceful manner,
        // we pass a cancellation token that we trigger when the future is dropped.
        // Dropping the future will block until the inner operation is finished.
        let token = CancellationToken::new();

        struct CompleteCancelledBlocking<Func, Fut>
        where
            Func: RemoteFunction,
            Fut: Future<Output = anyhow::Result<Func::Result>>,
        {
            token: CancellationToken,
            future: Option<Pin<Box<Fut>>>,
            marker: PhantomData<Func>,
        }

        impl<Func, Fut> Drop for CompleteCancelledBlocking<Func, Fut>
        where
            Func: RemoteFunction,
            Fut: Future<Output = anyhow::Result<Func::Result>>,
        {
            fn drop(&mut self) {
                // Cancel the operation.
                self.token.cancel();

                // Wait for the task to finish.
                if let Some(handle) = self.future.take() {
                    tokio::task::block_in_place(|| Handle::current().block_on(handle).unwrap());
                }
            }
        }

        let ctx = Context::new(
            self,
            move |msg: Vec<u8>| {
                match postcard::from_bytes(&msg) {
                    Ok(msg) => on_msg(msg),
                    Err(err) => tracing::error!("Failed to parse message: {err}"),
                };
            },
            token.clone(),
        );

        let mut completer = CompleteCancelledBlocking {
            token,
            future: Some(Box::pin(async { func.run(ctx).await })),
            marker: PhantomData::<F>,
        };

        let ret = completer.future.as_mut().unwrap().await;

        // Prevent the Drop impl from resuming the completed future.
        std::mem::forget(completer);

        ret
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
    F: FnOnce(&mut LocalSession, UnboundedSender<M>, CancellationToken) -> anyhow::Result<R>
        + Send
        + 'static,
{
    let (iface, mut emitter, token) = ctx.split();
    let mut moved_iface = std::mem::replace(iface, LocalSession::new());

    // Create channel for events.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<M>();

    // Run the blocking function on a separate thread. Note that we can't stop this thread.
    let worker = tokio::task::spawn_blocking({
        let token = token.clone();
        move || {
            let rv = f(&mut moved_iface, tx, token);
            (moved_iface, rv)
        }
    });

    let emit_messages = async {
        // Catch events and emit them to the client.
        while let Some(event) = rx.recv().await {
            emitter.emit(event).await?;
        }
        Ok::<_, anyhow::Error>(())
    };

    // If the token is cancelled, the message loop will be dropped and the channel closed.
    // This will cause the worker to panic when trying to send a message, effectively aborting it.
    tokio::select! {
        _ = token.cancelled() => {}
        _ = emit_messages => {}
    }

    let (moved_iface, rv) = worker.await?;

    *iface = moved_iface;

    rv
}
