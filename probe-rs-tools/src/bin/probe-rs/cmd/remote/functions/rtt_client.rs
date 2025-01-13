use std::path::{Path, PathBuf};

use crate::{
    cmd::remote::{
        functions::{Context, EmitterFn, RemoteFunctions},
        Key,
    },
    util::rtt::{client::RttClient, RttChannelConfig, RttConfig},
};
use probe_rs::{flashing::FormatKind, rtt::ScanRegion, Session};
use serde::{Deserialize, Serialize};

#[derive(Default, Debug, Serialize, Deserialize)]
pub struct LogOptions {
    /// Suppress filename and line number information from the rtt log
    pub no_location: bool,

    /// The format string to use when printing defmt encoded log messages from the target.
    ///
    /// See https://defmt.ferrous-systems.com/custom-log-output
    pub log_format: Option<String>,

    /// Scan the memory to find the RTT control block
    pub rtt_scan_memory: bool,
}

#[derive(Serialize, Deserialize)]
pub(in crate::cmd::remote) struct CreateRttClient {
    pub sessid: Key<Session>,
    pub path: Option<PathBuf>,
    pub log_options: LogOptions,
}

impl super::RemoteFunction for CreateRttClient {
    type Message = super::NoMessage;
    type Result = Key<RttClient>;

    async fn run(self, mut ctx: Context<'_, impl EmitterFn>) -> anyhow::Result<Key<RttClient>> {
        let client = {
            let mut session = ctx.session(self.sessid).await;
            create_rtt_client(&mut session, self.path.as_deref(), &self.log_options).await?
        };

        Ok(ctx.store_object(client))
    }
}

impl From<CreateRttClient> for RemoteFunctions {
    fn from(func: CreateRttClient) -> Self {
        RemoteFunctions::CreateRttClient(func)
    }
}

pub async fn create_rtt_client(
    session: &mut Session,
    path: Option<&Path>,
    log_options: &LogOptions,
) -> anyhow::Result<RttClient> {
    let rtt_scan_regions = match log_options.rtt_scan_memory {
        true => session.target().rtt_scan_regions.clone(),
        false => ScanRegion::Ranges(vec![]),
    };

    let mut rtt_config = RttConfig::default();
    rtt_config.channels.push(RttChannelConfig {
        channel_number: Some(0),
        show_location: !log_options.no_location,
        log_format: log_options.log_format.clone(),
        ..Default::default()
    });

    //let format = self.options.format_options.to_format_kind(session.target());
    let elf = if let Some(path) = path {
        let format = FormatKind::from_optional(session.target().default_format.as_deref())
            .expect("Failed to parse a default binary format. This shouldn't happen.");
        if matches!(format, FormatKind::Elf | FormatKind::Idf) {
            Some(tokio::fs::read(path).await?)
        } else {
            None
        }
    } else {
        None
    };

    Ok(RttClient::new(
        elf.as_deref(),
        session.target(),
        rtt_config,
        rtt_scan_regions,
    )?)
}
