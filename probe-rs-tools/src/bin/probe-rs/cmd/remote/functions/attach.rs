use serde::{Deserialize, Serialize};

use crate::{
    cmd::remote::{
        functions::{
            list_probes::DebugProbeEntry, Context, EmitterFn, RemoteFunction, RemoteFunctions,
        },
        SessionId,
    },
    util::common_options::{OperationError, ProbeOptions},
};

#[derive(Serialize, Deserialize)]
pub enum AttachResult {
    Success(SessionId),
    MultipleProbes(Vec<DebugProbeEntry>),
}

#[derive(Serialize, Deserialize)]
pub(in crate::cmd::remote) struct Attach {
    pub probe_options: ProbeOptions,
    pub resume_target: bool,
}

impl RemoteFunction for Attach {
    type Message = super::NoMessage;
    type Result = AttachResult;

    async fn run(mut self, mut ctx: Context<'_, impl EmitterFn>) -> anyhow::Result<AttachResult> {
        self.probe_options.non_interactive = true;

        let common_options = self.probe_options.load()?;
        let target = common_options.get_target_selector()?;

        match common_options.attach_probe(&ctx.lister()) {
            Ok(probe) => {
                let mut session = common_options.attach_session(probe, target)?;
                if self.resume_target {
                    session.resume_all_cores()?;
                }
                let session_id = ctx.set_session(session, common_options.dry_run());
                Ok(AttachResult::Success(session_id))
            }
            Err(OperationError::MultipleProbesFound { list }) => Ok(AttachResult::MultipleProbes(
                list.into_iter().map(DebugProbeEntry::from).collect(),
            )),
            Err(other) => anyhow::bail!(other),
        }
    }
}

impl From<Attach> for RemoteFunctions {
    fn from(func: Attach) -> Self {
        RemoteFunctions::Attach(func)
    }
}
