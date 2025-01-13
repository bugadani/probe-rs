use std::{cell::Cell, path::PathBuf, rc::Rc};

use probe_rs::{
    flashing::{BootInfo, FileDownloadError, FlashLayout, FlashProgress, ProgressEvent},
    rtt::ScanRegion,
    Session,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::{
    cmd::remote::{
        functions::{Context, EmitterFn, RemoteFunction, RemoteFunctions},
        run_blocking_streaming, Key, LocalSession,
    },
    util::{flash::build_loader, rtt::client::RttClient},
    FormatOptions,
};

#[derive(Serialize, Deserialize, Default)]
pub struct DownloadOptions {
    /// If `keep_unwritten_bytes` is `true`, erased portions of the flash that are not overwritten by the ELF data
    /// are restored afterwards, such that the old contents are untouched.
    ///
    /// This is necessary because the flash can only be erased in sectors. If only parts of the erased sector are written thereafter,
    /// instead of the full sector, the excessively erased bytes wont match the contents before the erase which might not be intuitive
    /// to the user or even worse, result in unexpected behavior if those contents contain important data.
    pub keep_unwritten_bytes: bool,
    /// If this flag is set to true, probe-rs will try to use the chips built in method to do a full chip erase if one is available.
    /// This is often faster than erasing a lot of single sectors.
    /// So if you do not need the old contents of the flash, this is a good option.
    pub do_chip_erase: bool,
    /// If the chip was pre-erased with external erasers, this flag can set to true to skip erasing
    /// It may be useful for mass production.
    pub skip_erase: bool,
    /// Before flashing, read back the flash contents to skip up-to-date regions.
    pub preverify: bool,
    /// After flashing, read back all the flashed data to verify it has been written correctly.
    pub verify: bool,
    /// Disable double buffering when loading flash.
    pub disable_double_buffering: bool,
}

#[derive(Serialize, Deserialize)]
pub(in crate::cmd::remote) struct Flash {
    pub sessid: Key<Session>,
    pub path: PathBuf,
    pub format: FormatOptions,
    pub options: DownloadOptions,
    pub rtt_client: Option<Key<RttClient>>,
}

#[derive(Serialize, Deserialize)]
pub struct FlashResult {
    pub boot_info: BootInfo,
    pub flash_layout: Vec<FlashLayout>,
}

impl RemoteFunction for Flash {
    type Message = ProgressEvent;
    type Result = FlashResult;

    async fn run(self, ctx: Context<'_, impl EmitterFn>) -> anyhow::Result<FlashResult> {
        let dry_run = ctx.dry_run(self.sessid);

        run_blocking_streaming(
            ctx,
            move |iface: &mut LocalSession,
                  tx: UnboundedSender<ProgressEvent>,
                  _token: CancellationToken|
                  -> anyhow::Result<FlashResult> {
                let mut session = iface.session_blocking(self.sessid);

                let mut rtt_client = self
                    .rtt_client
                    .map(|rtt_client| iface.object_mut_blocking(rtt_client));

                // build loader
                let loader = build_loader(&mut session, &self.path, self.format, None)?;

                // When using RTT with a program in flash, the RTT header will be moved to RAM on
                // startup, so clearing it before startup is ok. However, if we're downloading to the
                // header's final address in RAM, then it's not relocated on startup and we should not
                // clear it. This impacts static RTT headers, like used in defmt_rtt.
                let should_clear_rtt_header = if let Some(rtt_client) = rtt_client.as_ref() {
                    if let ScanRegion::Exact(address) = rtt_client.scan_region {
                        tracing::debug!(
                            "RTT ScanRegion::Exact address is within region to be flashed"
                        );
                        !loader.has_data_for_address(address)
                    } else {
                        true
                    }
                } else {
                    false
                };

                let flash_layout = Rc::new(Cell::new(vec![]));

                let mut options = probe_rs::flashing::DownloadOptions::default();

                options.keep_unwritten_bytes = self.options.keep_unwritten_bytes;
                options.dry_run = dry_run;
                options.do_chip_erase = self.options.do_chip_erase;
                options.skip_erase = self.options.skip_erase;
                options.preverify = self.options.preverify;
                options.verify = self.options.verify;
                options.disable_double_buffering = self.options.disable_double_buffering;
                options.progress = Some(FlashProgress::new({
                    let flash_layout = flash_layout.clone();
                    move |event| {
                        if let ProgressEvent::Initialized { ref phases, .. } = event {
                            flash_layout.set(phases.clone());
                        }
                        // The loader runs on a separate thread and emits progress events into a queue.
                        _ = tx.send(event);
                    }
                }));

                // run flash download
                loader
                    .commit(&mut session, options)
                    .map_err(FileDownloadError::Flash)?;

                let boot_info = loader.boot_info();

                if let Some(rtt_client) = rtt_client.as_mut() {
                    if should_clear_rtt_header {
                        // We ended up resetting the MCU, throw away old RTT data and prevent
                        // printing warnings when it initialises.
                        let mut core = session.core(rtt_client.core_id())?;
                        rtt_client.clear_control_block(&mut core)?;
                        tracing::debug!("Cleared RTT header");
                    }
                }

                Ok::<_, anyhow::Error>(FlashResult {
                    boot_info,
                    flash_layout: flash_layout.take(),
                })
            },
        )
        .await
    }
}

impl From<Flash> for RemoteFunctions {
    fn from(func: Flash) -> Self {
        RemoteFunctions::Flash(func)
    }
}
