use std::{any::Any, ops::DerefMut};

use probe_rs::{probe::list::Lister, MemoryInterface, Session};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

#[cfg(feature = "remote")]
use crate::cmd::remote::RemoteSession;
use crate::cmd::remote::{Key, LocalSession};

pub mod attach;
pub mod chip;
pub mod flash;
pub mod info;
pub mod list_probes;
pub mod monitor;
pub mod read_memory;
pub mod reset;
pub mod resume;
pub mod stack_trace;
pub mod test;
pub mod write_memory;

#[derive(Serialize, Deserialize)]
pub(super) enum NoMessage {}

pub(super) trait RemoteFunction: Serialize + Into<RemoteFunctions> + Send {
    type Result: DeserializeOwned + Send;
    type Message: DeserializeOwned + Send;

    #[cfg(feature = "remote")]
    async fn prepare_remote(&mut self, _iface: &mut RemoteSession) -> anyhow::Result<()> {
        Ok(())
    }

    async fn run(self, ctx: Context<'_, impl EmitterFn>) -> anyhow::Result<Self::Result>;
}

/// The functions that can be called remotely.
#[derive(Serialize, Deserialize)]
pub(super) enum RemoteFunctions {
    Attach(attach::Attach),
    ListProbes(list_probes::ListProbes),
    ReadMemory8(read_memory::ReadMemory<u8>),
    ReadMemory16(read_memory::ReadMemory<u16>),
    ReadMemory32(read_memory::ReadMemory<u32>),
    ReadMemory64(read_memory::ReadMemory<u64>),
    WriteMemory8(write_memory::WriteMemory<u8>),
    WriteMemory16(write_memory::WriteMemory<u16>),
    WriteMemory32(write_memory::WriteMemory<u32>),
    WriteMemory64(write_memory::WriteMemory<u64>),
    ResumeAllCores(resume::ResumeAllCores),
    ResetCore(reset::ResetCore),
    ListChipFamilies(chip::ListFamilies),
    ChipInfo(chip::ChipInfo),
    LoadChipFamilies(chip::LoadChipFamilies),
    Info(info::Info),
    Flash(flash::Flash),
    Monitor(monitor::Monitor),
    ListTests(test::ListTests),
    RunTest(test::RunTest),
    StackTrace(stack_trace::TakeStackTrace),
}

pub trait EmitterFn: Send {
    fn call(
        &mut self,
        args: Vec<u8>,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
}

impl<F: FnMut(Vec<u8>) + Send> EmitterFn for F {
    async fn call(&mut self, args: Vec<u8>) -> anyhow::Result<()> {
        self(args);
        Ok(())
    }
}

pub struct Emitter<F>(F);

impl<F: EmitterFn> Emitter<F> {
    pub async fn emit(&mut self, data: impl Serialize) -> anyhow::Result<()> {
        let data = postcard::to_stdvec(&data)?;
        self.0.call(data).await
    }
}

pub struct Context<'a, F> {
    iface: &'a mut LocalSession,
    emitter: F,
    token: CancellationToken,
}

impl<'a, F: EmitterFn> Context<'a, F> {
    pub fn new(iface: &'a mut LocalSession, emitter: F, token: CancellationToken) -> Self {
        Self {
            iface,
            emitter,
            token,
        }
    }

    pub async fn emit(&mut self, data: impl Serialize) -> anyhow::Result<()> {
        let data = postcard::to_stdvec(&data)?;
        self.emitter.call(data).await
    }

    pub async fn object_mut<T: Any + Send>(&self, key: Key<T>) -> impl DerefMut<Target = T> + Send {
        self.iface.object_mut(key).await
    }

    pub fn set_session(&mut self, session: Session, dry_run: bool) -> Key<Session> {
        self.iface.set_session(session, dry_run)
    }

    pub async fn session(&mut self, sid: Key<Session>) -> impl DerefMut<Target = Session> + Send {
        self.object_mut(sid).await
    }

    pub fn dry_run(&self, sid: Key<Session>) -> bool {
        self.iface.dry_run(sid)
    }

    pub fn lister(&self) -> Lister {
        self.iface.lister()
    }

    pub fn split(self) -> (&'a mut LocalSession, Emitter<F>, CancellationToken) {
        (self.iface, Emitter(self.emitter), self.token)
    }
}

impl RemoteFunctions {
    #[cfg(feature = "remote")]
    pub async fn run(self, ctx: Context<'_, impl EmitterFn>) -> anyhow::Result<Vec<u8>> {
        let result = match self {
            RemoteFunctions::Attach(func) => postcard::to_stdvec(&func.run(ctx).await?),
            RemoteFunctions::ListProbes(func) => postcard::to_stdvec(&func.run(ctx).await?),
            RemoteFunctions::ReadMemory8(func) => postcard::to_stdvec(&func.run(ctx).await?),
            RemoteFunctions::ReadMemory16(func) => postcard::to_stdvec(&func.run(ctx).await?),
            RemoteFunctions::ReadMemory32(func) => postcard::to_stdvec(&func.run(ctx).await?),
            RemoteFunctions::ReadMemory64(func) => postcard::to_stdvec(&func.run(ctx).await?),
            RemoteFunctions::WriteMemory8(func) => postcard::to_stdvec(&func.run(ctx).await?),
            RemoteFunctions::WriteMemory16(func) => postcard::to_stdvec(&func.run(ctx).await?),
            RemoteFunctions::WriteMemory32(func) => postcard::to_stdvec(&func.run(ctx).await?),
            RemoteFunctions::WriteMemory64(func) => postcard::to_stdvec(&func.run(ctx).await?),
            RemoteFunctions::ResumeAllCores(func) => postcard::to_stdvec(&func.run(ctx).await?),
            RemoteFunctions::ResetCore(func) => postcard::to_stdvec(&func.run(ctx).await?),
            RemoteFunctions::ListChipFamilies(func) => postcard::to_stdvec(&func.run(ctx).await?),
            RemoteFunctions::ChipInfo(func) => postcard::to_stdvec(&func.run(ctx).await?),
            RemoteFunctions::LoadChipFamilies(func) => postcard::to_stdvec(&func.run(ctx).await?),
            RemoteFunctions::Info(func) => postcard::to_stdvec(&func.run(ctx).await?),
            RemoteFunctions::Flash(func) => postcard::to_stdvec(&func.run(ctx).await?),
            RemoteFunctions::Monitor(func) => postcard::to_stdvec(&func.run(ctx).await?),
            RemoteFunctions::ListTests(func) => postcard::to_stdvec(&func.run(ctx).await?),
            RemoteFunctions::RunTest(func) => postcard::to_stdvec(&func.run(ctx).await?),
            RemoteFunctions::StackTrace(func) => postcard::to_stdvec(&func.run(ctx).await?),
        };

        result.map_err(|e| e.into())
    }
}

pub trait Word: Copy + Default + Send {
    fn read(
        core: &mut impl MemoryInterface,
        address: u64,
        out: &mut Vec<Self>,
    ) -> anyhow::Result<()>;

    fn write(core: &mut impl MemoryInterface, address: u64, data: &[Self]) -> anyhow::Result<()>;
}

impl Word for u8 {
    fn read(
        core: &mut impl MemoryInterface,
        address: u64,
        out: &mut Vec<Self>,
    ) -> anyhow::Result<()> {
        core.read_8(address, out)?;
        Ok(())
    }

    fn write(core: &mut impl MemoryInterface, address: u64, data: &[Self]) -> anyhow::Result<()> {
        core.write_8(address, data)?;
        Ok(())
    }
}
impl Word for u16 {
    fn read(
        core: &mut impl MemoryInterface,
        address: u64,
        out: &mut Vec<Self>,
    ) -> anyhow::Result<()> {
        core.read_16(address, out)?;
        Ok(())
    }

    fn write(core: &mut impl MemoryInterface, address: u64, data: &[Self]) -> anyhow::Result<()> {
        core.write_16(address, data)?;
        Ok(())
    }
}
impl Word for u32 {
    fn read(
        core: &mut impl MemoryInterface,
        address: u64,
        out: &mut Vec<Self>,
    ) -> anyhow::Result<()> {
        core.read_32(address, out)?;
        Ok(())
    }

    fn write(core: &mut impl MemoryInterface, address: u64, data: &[Self]) -> anyhow::Result<()> {
        core.write_32(address, data)?;
        Ok(())
    }
}
impl Word for u64 {
    fn read(
        core: &mut impl MemoryInterface,
        address: u64,
        out: &mut Vec<Self>,
    ) -> anyhow::Result<()> {
        core.read_64(address, out)?;
        Ok(())
    }

    fn write(core: &mut impl MemoryInterface, address: u64, data: &[Self]) -> anyhow::Result<()> {
        core.write_64(address, data)?;
        Ok(())
    }
}
