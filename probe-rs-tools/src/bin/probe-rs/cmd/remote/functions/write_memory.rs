use crate::cmd::remote::{
    functions::{Context, EmitterFn, RemoteFunctions, Word},
    Key,
};
use probe_rs::Session;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub(in crate::cmd::remote) struct WriteMemory<W: Word> {
    pub sessid: Key<Session>,
    pub core: usize,
    pub address: u64,
    pub data: Vec<W>,
}

impl<W> super::RemoteFunction for WriteMemory<W>
where
    W: Word + Serialize,
    RemoteFunctions: From<Self>,
{
    type Message = super::NoMessage;
    type Result = ();

    async fn run(self, mut ctx: Context<'_, impl EmitterFn>) -> anyhow::Result<()> {
        let mut session = ctx.session(self.sessid).await;
        let mut core = session.core(self.core).unwrap();
        W::write(&mut core, self.address, &self.data)?;
        Ok(())
    }
}

impl From<WriteMemory<u8>> for RemoteFunctions {
    fn from(func: WriteMemory<u8>) -> Self {
        RemoteFunctions::WriteMemory8(func)
    }
}

impl From<WriteMemory<u16>> for RemoteFunctions {
    fn from(func: WriteMemory<u16>) -> Self {
        RemoteFunctions::WriteMemory16(func)
    }
}

impl From<WriteMemory<u32>> for RemoteFunctions {
    fn from(func: WriteMemory<u32>) -> Self {
        RemoteFunctions::WriteMemory32(func)
    }
}

impl From<WriteMemory<u64>> for RemoteFunctions {
    fn from(func: WriteMemory<u64>) -> Self {
        RemoteFunctions::WriteMemory64(func)
    }
}
