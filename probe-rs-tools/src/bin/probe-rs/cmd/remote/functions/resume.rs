use crate::cmd::remote::{
    functions::{Context, EmitterFn, RemoteFunctions},
    SessionId,
};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub(in crate::cmd::remote) struct ResumeAllCores {
    pub sessid: SessionId,
}

impl super::RemoteFunction for ResumeAllCores {
    type Message = super::NoMessage;
    type Result = ();

    async fn run(self, mut ctx: Context<'_, impl EmitterFn>) -> anyhow::Result<()> {
        ctx.session(self.sessid).resume_all_cores()?;
        Ok(())
    }
}

impl From<ResumeAllCores> for RemoteFunctions {
    fn from(func: ResumeAllCores) -> Self {
        RemoteFunctions::ResumeAllCores(func)
    }
}
