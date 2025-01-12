use crate::cmd::remote::{
    functions::{Context, EmitterFn, RemoteFunctions},
    Key,
};
use probe_rs::Session;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub(in crate::cmd::remote) struct ResetCore {
    pub sessid: Key<Session>,
    pub core: usize,
}

impl super::RemoteFunction for ResetCore {
    type Message = super::NoMessage;
    type Result = ();

    async fn run(self, mut ctx: Context<'_, impl EmitterFn>) -> anyhow::Result<()> {
        let mut session = ctx.session(self.sessid).await;
        let mut core = session.core(self.core)?;
        core.reset()?;
        Ok(())
    }
}

impl From<ResetCore> for RemoteFunctions {
    fn from(func: ResetCore) -> Self {
        RemoteFunctions::ResetCore(func)
    }
}
