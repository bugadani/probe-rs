use std::{fmt::Write, num::NonZeroU32, time::Duration};

use crate::{
    cmd::{
        remote::{
            functions::{Context, EmitterFn, RemoteFunctions},
            run_blocking_streaming, Key, LocalSession,
        },
        run::RunLoop,
    },
    util::rtt::client::RttClient,
};
use probe_rs::{
    flashing::BootInfo, semihosting::SemihostingCommand, BreakpointCause, Core, HaltReason, Session,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

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
    /// RTT client if used.
    pub rtt_client: Option<Key<RttClient>>,
}

/// Monitor in normal run mode.
#[derive(Serialize, Deserialize)]
pub(in crate::cmd::remote) struct Monitor {
    pub sessid: Key<Session>,
    pub mode: MonitorMode,
    pub options: MonitorOptions,
}

impl super::RemoteFunction for Monitor {
    type Message = MonitorEvent;
    type Result = ();

    async fn run(self, ctx: Context<'_, impl EmitterFn>) -> anyhow::Result<()> {
        run_blocking_streaming(
            ctx,
            move |iface: &mut LocalSession,
                  tx: UnboundedSender<MonitorEvent>,
                  cancellation_token: CancellationToken|
                  -> anyhow::Result<()> {
                let mut session = iface.session_blocking(self.sessid);
                let mut semihosting_sink = MonitorEventHandler::new(tx.clone());
                let mut rtt_sink =
                    RttStreamer::new(tx, cancellation_token.clone(), MonitorEvent::RttOutput);

                let mut rtt_client = self
                    .options
                    .rtt_client
                    .map(|rtt_client| iface.object_mut_blocking(rtt_client));

                let core_id = rtt_client.as_ref().map(|rtt| rtt.core_id()).unwrap_or(0);

                let mut run_loop = RunLoop {
                    core_id,
                    rtt_client: rtt_client.as_deref_mut(),
                    cancellation_token,
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
                    &mut rtt_sink,
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

pub struct RttStreamer<E> {
    tx: UnboundedSender<E>,
    transform: fn(String) -> E,
    buffer: String,
    cancellation_token: CancellationToken,
}

impl<E> RttStreamer<E> {
    pub fn new(
        tx: UnboundedSender<E>,
        cancellation_token: CancellationToken,
        transform: fn(String) -> E,
    ) -> Self {
        Self {
            tx,
            transform,
            cancellation_token,
            buffer: String::new(),
        }
    }
}

impl<E> Write for RttStreamer<E> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        if self.cancellation_token.is_cancelled() {
            return Ok(());
        }
        self.buffer.push_str(s);

        // Send whole lines
        while let Some(pos) = self.buffer.find('\n') {
            let line = self.buffer.drain(..pos + 1).collect();
            self.tx
                .send((self.transform)(line))
                .map_err(|_| std::fmt::Error)?;
        }

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
