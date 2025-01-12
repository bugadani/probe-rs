use std::marker::PhantomData;

use crate::cmd::remote::{
    functions::{Context, EmitterFn, RemoteFunctions, Word},
    Key,
};
use probe_rs::Session;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub(in crate::cmd::remote) struct ReadMemory<T: Word> {
    pub sessid: Key<Session>,
    pub core: usize,
    pub address: u64,
    pub count: usize,
    pub _phantom: PhantomData<T>,
}

impl<W> super::RemoteFunction for ReadMemory<W>
where
    W: Word + DeserializeOwned,
    RemoteFunctions: From<Self>,
{
    type Message = super::NoMessage;
    type Result = Vec<W>;

    async fn run(self, mut ctx: Context<'_, impl EmitterFn>) -> anyhow::Result<Vec<W>> {
        let mut session = ctx.session(self.sessid).await;
        let mut core = session.core(self.core)?;

        let mut words = vec![W::default(); self.count];
        W::read(&mut core, self.address, &mut words)?;
        Ok(words)
    }
}

impl From<ReadMemory<u8>> for RemoteFunctions {
    fn from(func: ReadMemory<u8>) -> Self {
        RemoteFunctions::ReadMemory8(func)
    }
}

impl From<ReadMemory<u16>> for RemoteFunctions {
    fn from(func: ReadMemory<u16>) -> Self {
        RemoteFunctions::ReadMemory16(func)
    }
}

impl From<ReadMemory<u32>> for RemoteFunctions {
    fn from(func: ReadMemory<u32>) -> Self {
        RemoteFunctions::ReadMemory32(func)
    }
}

impl From<ReadMemory<u64>> for RemoteFunctions {
    fn from(func: ReadMemory<u64>) -> Self {
        RemoteFunctions::ReadMemory64(func)
    }
}
