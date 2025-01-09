use crate::{
    cmd::remote::ClientInterface,
    util::{cli, common_options::ProbeOptions},
    CoreOptions,
};

#[derive(clap::Parser)]
pub struct Cmd {
    #[clap(flatten)]
    shared: CoreOptions,

    #[clap(flatten)]
    common: ProbeOptions,
}

impl Cmd {
    pub async fn run(self, mut iface: impl ClientInterface) -> anyhow::Result<()> {
        let mut session = cli::attach_probe(&mut iface, self.common, false).await?;
        let mut core = session.core(self.shared.core);

        core.reset().await?;

        Ok(())
    }
}
