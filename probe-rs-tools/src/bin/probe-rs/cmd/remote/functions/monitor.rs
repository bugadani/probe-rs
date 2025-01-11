use std::{
    io::Write,
    num::NonZeroU32,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{
    cmd::{
        remote::{
            functions::{Context, EmitterFn, RemoteFunctions},
            run_blocking_streaming, LocalSession, SessionId,
        },
        run::{OutputStream, RunLoop},
    },
    util::rtt::{client::RttClient, RttChannelConfig, RttConfig},
};
use probe_rs::{
    flashing::{BootInfo, FormatKind},
    rtt::ScanRegion,
    semihosting::SemihostingCommand,
    BreakpointCause, Core, HaltReason, Session,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

#[cfg(feature = "remote")]
use crate::cmd::remote::RemoteSession;

#[derive(Serialize, Deserialize)]
pub enum MonitorMode {
    AttachToRunning,
    Run(BootInfo),
}

#[derive(Serialize, Deserialize)]
pub enum MonitorEvent {
    RttOutput(String), // Monitor supports a single channel only
    SemihostingOutput(SemihostingOutput),
}

#[derive(Serialize, Deserialize)]
pub struct MonitorOptions {
    /// Enable reset vector catch if its supported on the target.
    pub catch_reset: bool,
    /// Enable hardfault vector catch if its supported on the target.
    pub catch_hardfault: bool,
    pub log: LogOptions,
}

#[derive(Debug, Serialize, Deserialize)]
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

/// Monitor in normal run mode.
#[derive(Serialize, Deserialize)]
pub(in crate::cmd::remote) struct Monitor {
    pub sessid: SessionId,
    pub mode: MonitorMode,
    /// The path to the ELF file to flash and run.
    pub path: PathBuf,
    pub options: MonitorOptions,
}

impl super::RemoteFunction for Monitor {
    type Message = MonitorEvent;
    type Result = ();

    #[cfg(feature = "remote")]
    async fn prepare_remote(&mut self, iface: &mut RemoteSession) -> anyhow::Result<()> {
        // TODO: avoid temp files
        self.path = iface.upload_file(&self.path).await?;

        Ok(())
    }

    async fn run(self, mut ctx: Context<'_, impl EmitterFn>) -> anyhow::Result<()> {
        let session = ctx.session(self.sessid);

        // For normal run mode, we expect the RTT header to be cleared if necessary by the flashing process.
        let rtt_client = create_rtt_client(session, &self.path, &self.options.log).await?;

        run_blocking_streaming(
            ctx,
            move |iface: &mut LocalSession,
                  tx: UnboundedSender<MonitorEvent>|
                  -> anyhow::Result<()> {
                let session = iface.session(self.sessid);
                let mut semihosting_sink = MonitorEventHandler::new(tx.clone());
                let rtt_sink = Box::new(RttStreamer::new(tx, MonitorEvent::RttOutput));
                let core_id = rtt_client.core_id();

                let mut run_loop = RunLoop {
                    core_id,
                    path: self.path,
                    always_print_stacktrace: false, // impl as a separate function
                    rtt_client,
                };

                let monitor_mode = if session.core(core_id)?.core_halted()? {
                    self.mode
                } else {
                    // Core is running so we can ignore BootInfo
                    MonitorMode::AttachToRunning
                };

                match monitor_mode {
                    MonitorMode::Run(BootInfo::FromRam {
                        vector_table_addr, ..
                    }) => {
                        // core should be already reset and halt by this point.
                        session.prepare_running_on_ram(vector_table_addr)?;
                    }
                    MonitorMode::Run(BootInfo::Other) => {
                        // reset the core to leave it in a consistent state after flashing
                        session
                            .core(core_id)?
                            .reset_and_halt(Duration::from_millis(100))?;
                    }
                    MonitorMode::AttachToRunning => {
                        // do nothing
                    }
                }

                let mut core = session.core(run_loop.core_id)?;
                run_loop.run_until(
                    &mut core,
                    self.options.catch_hardfault,
                    self.options.catch_reset,
                    OutputStream::Writer(rtt_sink),
                    None,
                    |halt_reason, core| semihosting_sink.handle_halt(halt_reason, core),
                )?;

                Ok(())
            },
        )
        .await
    }
}

impl From<Monitor> for RemoteFunctions {
    fn from(func: Monitor) -> Self {
        RemoteFunctions::Monitor(func)
    }
}

pub async fn create_rtt_client(
    session: &mut Session,
    path: &Path,
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
    let format = FormatKind::from_optional(session.target().default_format.as_deref())
        .expect("Failed to parse a default binary format. This shouldn't happen.");
    let elf = if matches!(format, FormatKind::Elf | FormatKind::Idf) {
        Some(tokio::fs::read(path).await?)
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

pub struct RttStreamer<E> {
    tx: UnboundedSender<E>,
    transform: fn(String) -> E,
    buffer: String,
}

impl<E> RttStreamer<E> {
    pub fn new(tx: UnboundedSender<E>, transform: fn(String) -> E) -> Self {
        Self {
            tx,
            transform,
            buffer: String::new(),
        }
    }
}

impl<E> Write for RttStreamer<E> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buffer.push_str(&String::from_utf8_lossy(buf));

        // Send whole lines
        while let Some(pos) = self.buffer.find('\n') {
            let line = self.buffer.drain(..pos + 1).collect();
            self.tx.send((self.transform)(line)).unwrap();
        }

        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let str = std::mem::take(&mut self.buffer);
        self.tx.send((self.transform)(str)).unwrap();
        Ok(())
    }
}

struct MonitorEventHandler {
    tx: UnboundedSender<MonitorEvent>,
    semihosting_reader: SemihostingReader,
}

impl MonitorEventHandler {
    pub fn new(tx: UnboundedSender<MonitorEvent>) -> Self {
        Self {
            tx,
            semihosting_reader: SemihostingReader::new(),
        }
    }

    fn handle_halt(
        &mut self,
        halt_reason: HaltReason,
        core: &mut Core<'_>,
    ) -> anyhow::Result<Option<()>> {
        let HaltReason::Breakpoint(BreakpointCause::Semihosting(cmd)) = halt_reason else {
            anyhow::bail!("CPU halted unexpectedly.");
        };

        match cmd {
            SemihostingCommand::ExitSuccess => Ok(Some(())), // Exit the run loop
            SemihostingCommand::ExitError(details) => {
                Err(anyhow::anyhow!("Semihosting indicated exit with {details}"))
            }
            SemihostingCommand::Unknown(details) => {
                tracing::warn!(
                    "Target wanted to run semihosting operation {:#x} with parameter {:#x},\
                     but probe-rs does not support this operation yet. Continuing...",
                    details.operation,
                    details.parameter
                );
                Ok(None) // Continue running
            }
            SemihostingCommand::GetCommandLine(_) => {
                tracing::warn!("Target wanted to run semihosting operation SYS_GET_CMDLINE, but probe-rs does not support this operation yet. Continuing...");
                Ok(None) // Continue running
            }
            other @ (SemihostingCommand::Open(_)
            | SemihostingCommand::Close(_)
            | SemihostingCommand::WriteConsole(_)
            | SemihostingCommand::Write(_)) => {
                if let Some(output) = self.semihosting_reader.handle(other, core)? {
                    self.tx.send(MonitorEvent::SemihostingOutput(output))?;
                }
                Ok(None)
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
pub enum SemihostingOutput {
    StdOut(String),
    StdErr(String),
}

pub struct SemihostingReader {
    stdout_open: bool,
    stderr_open: bool,
}

impl SemihostingReader {
    const STDOUT: NonZeroU32 = NonZeroU32::new(1).unwrap();
    const STDERR: NonZeroU32 = NonZeroU32::new(2).unwrap();

    pub fn new() -> Self {
        Self {
            stdout_open: false,
            stderr_open: false,
        }
    }

    pub fn handle(
        &mut self,
        command: SemihostingCommand,
        core: &mut Core<'_>,
    ) -> anyhow::Result<Option<SemihostingOutput>> {
        let out = match command {
            SemihostingCommand::Open(request) => {
                let path = request.path(core)?;
                if path == ":tt" {
                    match request.mode().as_bytes()[0] {
                        b'w' => {
                            self.stdout_open = true;
                            request.respond_with_handle(core, Self::STDOUT)?;
                        }
                        b'a' => {
                            self.stderr_open = true;
                            request.respond_with_handle(core, Self::STDERR)?;
                        }
                        other => {
                            tracing::warn!(
                                "Target wanted to open file {path} with mode {mode}, but probe-rs does not support this operation yet. Continuing...",
                                path = path,
                                mode = other
                            );
                        }
                    };
                } else {
                    tracing::warn!(
                        "Target wanted to open file {path}, but probe-rs does not support this operation yet. Continuing..."
                    );
                }
                None
            }
            SemihostingCommand::Close(request) => {
                let handle = request.file_handle(core)?;
                if handle == Self::STDOUT.get() {
                    self.stdout_open = false;
                    request.success(core)?;
                } else if handle == Self::STDERR.get() {
                    self.stderr_open = false;
                    request.success(core)?;
                } else {
                    tracing::warn!(
                        "Target wanted to close file handle {handle}, but probe-rs does not support this operation yet. Continuing..."
                    );
                }
                None
            }
            SemihostingCommand::Write(request) => {
                let mut out = None;
                match request.file_handle() {
                    handle if handle == Self::STDOUT.get() => {
                        if self.stdout_open {
                            let bytes = request.read(core)?;
                            let str = String::from_utf8_lossy(&bytes);
                            out = Some(SemihostingOutput::StdOut(str.to_string()));
                            request.write_status(core, 0)?;
                        }
                    }
                    handle if handle == Self::STDERR.get() => {
                        if self.stderr_open {
                            let bytes = request.read(core)?;
                            let str = String::from_utf8_lossy(&bytes);
                            out = Some(SemihostingOutput::StdErr(str.to_string()));
                            request.write_status(core, 0)?;
                        }
                    }
                    other => {
                        tracing::warn!(
                            "Target wanted to write to file handle {other}, but probe-rs does not support this operation yet. Continuing...",
                        );
                    }
                }
                out
            }
            SemihostingCommand::WriteConsole(request) => {
                let str = request.read(core)?;
                Some(SemihostingOutput::StdOut(str))
            }

            _ => None,
        };

        Ok(out)
    }
}
