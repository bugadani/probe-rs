use crate::cmd::remote::{
    functions::{Context, EmitterFn, RemoteFunctions},
    SessionId,
};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub(in crate::cmd::remote) struct ResetCore {
    pub sessid: SessionId,
    pub core: usize,
}

impl super::RemoteFunction for ResetCore {
    type Message = super::NoMessage;
    type Result = ();

    async fn run(self, mut ctx: Context<'_, impl EmitterFn>) -> anyhow::Result<()> {
        let session = ctx.session(self.sessid);
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
