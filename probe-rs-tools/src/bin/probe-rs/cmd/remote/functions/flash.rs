use std::{cell::Cell, path::PathBuf, rc::Rc};

use probe_rs::{
    flashing::{BootInfo, FileDownloadError, FlashLayout, FlashProgress, ProgressEvent},
    rtt::ScanRegion,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    cmd::remote::{
        functions::{monitor::LogOptions, Context, EmitterFn, RemoteFunction, RemoteFunctions},
        run_blocking_streaming, LocalSession, SessionId,
    },
    util::flash::build_loader,
    FormatOptions,
};

#[cfg(feature = "remote")]
use crate::cmd::remote::RemoteSession;

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
    pub sessid: SessionId,
    pub path: PathBuf,
    pub format: FormatOptions,
    pub options: DownloadOptions,
}

#[derive(Serialize, Deserialize)]
pub struct FlashResult {
    pub boot_info: BootInfo,
    pub flash_layout: Vec<FlashLayout>,
}

impl RemoteFunction for Flash {
    type Message = ProgressEvent;
    type Result = FlashResult;

    #[cfg(feature = "remote")]
    async fn prepare_remote(&mut self, iface: &mut RemoteSession) -> anyhow::Result<()> {
        self.path = iface.upload_file(&self.path).await?;

        if let Some(ref mut idf_bootloader) = self.format.idf_options.idf_bootloader {
            *idf_bootloader = iface.upload_file(&*idf_bootloader).await?;
        }

        if let Some(ref mut idf_partition_table) = self.format.idf_options.idf_partition_table {
            *idf_partition_table = iface.upload_file(&*idf_partition_table).await?;
        }

        Ok(())
    }

    async fn run(self, mut ctx: Context<'_, impl EmitterFn>) -> anyhow::Result<FlashResult> {
        let dry_run = ctx.dry_run(self.sessid);

        let session = ctx.session(self.sessid);
        let mut rtt_client = super::monitor::create_rtt_client(
            session,
            &self.path,
            &LogOptions {
                no_location: false,
                log_format: None,
                rtt_scan_memory: false,
            },
        )
        .await?;

        run_blocking_streaming(
            ctx,
            move |iface: &mut LocalSession,
                  tx: UnboundedSender<ProgressEvent>|
                  -> anyhow::Result<FlashResult> {
                let session = iface.session(self.sessid);

                // build loader
                let loader = build_loader(session, &self.path, self.format, None)?;

                // When using RTT with a program in flash, the RTT header will be moved to RAM on
                // startup, so clearing it before startup is ok. However, if we're downloading to the
                // header's final address in RAM, then it's not relocated on startup and we should not
                // clear it. This impacts static RTT headers, like used in defmt_rtt.
                let should_clear_rtt_header = if let ScanRegion::Exact(address) =
                    rtt_client.scan_region
                {
                    tracing::debug!("RTT ScanRegion::Exact address is within region to be flashed");
                    !loader.has_data_for_address(address)
                } else {
                    true
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
                    .commit(session, options)
                    .map_err(FileDownloadError::Flash)?;

                let boot_info = loader.boot_info();

                if should_clear_rtt_header {
                    // We ended up resetting the MCU, throw away old RTT data and prevent
                    // printing warnings when it initialises.
                    let mut core = session.core(rtt_client.core_id())?;
                    rtt_client.clear_control_block(&mut core)?;
                    tracing::debug!("Cleared RTT header");
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
