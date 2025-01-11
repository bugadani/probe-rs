use crate::cmd::remote::functions::monitor::{LogOptions, MonitorMode, MonitorOptions};
use crate::cmd::remote::ClientInterface;
use crate::util::cli;

#[derive(clap::Parser)]
#[group(skip)]
pub struct Cmd {
    #[clap(flatten)]
    pub(crate) run: crate::cmd::run::Cmd,
}

impl Cmd {
    pub async fn run(self, mut iface: impl ClientInterface) -> anyhow::Result<()> {
        let mut session =
            cli::attach_probe(&mut iface, self.run.shared_options.probe_options, true).await?;

        cli::monitor(
            &mut session,
            MonitorMode::AttachToRunning,
            &self.run.shared_options.path,
            MonitorOptions {
                catch_reset: self.run.run_options.catch_reset,
                catch_hardfault: self.run.run_options.catch_hardfault,
                log: LogOptions {
                    no_location: self.run.shared_options.no_location,
                    log_format: self.run.shared_options.log_format,
                    rtt_scan_memory: self.run.shared_options.rtt_scan_memory,
                },
            },
            self.run.shared_options.always_print_stacktrace,
        )
        .await?;

        Ok(())
    }
}
