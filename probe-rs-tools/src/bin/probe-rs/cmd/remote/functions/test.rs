use std::{path::Path, time::Duration};

use probe_rs::{
    flashing::BootInfo, semihosting::SemihostingCommand, BreakpointCause, Core, HaltReason,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::cmd::{
    remote::{
        functions::{
            monitor::{
                create_rtt_client, LogOptions, RttStreamer, SemihostingOutput, SemihostingReader,
            },
            Context, EmitterFn, RemoteFunctions,
        },
        run_blocking_streaming, LocalSession, SessionId,
    },
    run::{print_stacktrace, ReturnReason, RunLoop},
};

#[derive(Debug, Serialize, Deserialize)]
pub struct Tests {
    pub version: u32,
    pub tests: Vec<Test>,
}

#[derive(PartialEq, Debug, Clone, Copy, Serialize, Deserialize)]
pub enum TestOutcome {
    Panic,
    Pass,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Test {
    pub name: String,
    #[serde(
        rename = "should_panic",
        deserialize_with = "outcome_from_should_panic"
    )]
    pub expected_outcome: TestOutcome,
    pub ignored: bool,
    pub timeout: Option<u32>,
}

fn outcome_from_should_panic<'de, D>(deserializer: D) -> Result<TestOutcome, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let should_panic = bool::deserialize(deserializer)?;
    Ok(if should_panic {
        TestOutcome::Panic
    } else {
        TestOutcome::Pass
    })
}

#[derive(Serialize, Deserialize)]
pub enum TestResult {
    Success,
    Failed(String, Option<String>),
    Cancelled,
}

#[derive(Serialize, Deserialize)]
pub enum TestEvent {
    RttOutput(String),
    SemihostingOutput(SemihostingOutput),
}

#[derive(Serialize, Deserialize)]
pub(in crate::cmd::remote) struct ListTests {
    pub sessid: SessionId,
    pub boot_info: BootInfo,
}

impl super::RemoteFunction for ListTests {
    type Message = super::NoMessage;
    type Result = Tests;

    async fn run(self, mut ctx: Context<'_, impl EmitterFn>) -> anyhow::Result<Tests> {
        let session = ctx.session(self.sessid);

        // TODO: add an RPC function to create RTT client and return a handle to it
        let rtt_client = create_rtt_client(session, None, &LogOptions::default()).await?;

        let core_id = rtt_client.core_id();

        match self.boot_info {
            BootInfo::FromRam {
                vector_table_addr, ..
            } => {
                // core should be already reset and halt by this point.
                session.prepare_running_on_ram(vector_table_addr)?;
            }
            BootInfo::Other => {
                // reset the core to leave it in a consistent state after flashing
                session
                    .core(core_id)?
                    .reset_and_halt(Duration::from_millis(100))?;
            }
        }

        run_blocking_streaming(
            ctx,
            move |iface: &mut LocalSession,
                  tx: UnboundedSender<TestEvent>,
                  cancellation_token: CancellationToken|
                  -> anyhow::Result<Tests> {
                let session = iface.session(self.sessid);
                let mut list_handler = ListEventHandler::new();
                let mut rtt_sink =
                    RttStreamer::new(tx, cancellation_token.clone(), TestEvent::RttOutput);

                let mut run_loop = RunLoop {
                    core_id,
                    rtt_client,
                    cancellation_token,
                };

                match run_loop.run_until(
                    &mut session.core(0)?,
                    true,
                    true,
                    &mut rtt_sink,
                    Some(Duration::from_secs(5)),
                    |halt_reason, core| list_handler.handle_halt(halt_reason, core),
                )? {
                    ReturnReason::Predicate(tests) => Ok(tests),
                    ReturnReason::Timeout => {
                        anyhow::bail!("The target did not respond with test list until timeout.")
                    }
                    ReturnReason::Cancelled => Ok(Tests {
                        version: 1,
                        tests: vec![],
                    }),
                }
            },
        )
        .await
    }
}

impl From<ListTests> for RemoteFunctions {
    fn from(func: ListTests) -> Self {
        RemoteFunctions::ListTests(func)
    }
}

#[derive(Serialize, Deserialize)]
pub(in crate::cmd::remote) struct RunTest {
    pub sessid: SessionId,
    pub test: Test,
}

impl super::RemoteFunction for RunTest {
    type Message = super::NoMessage;
    type Result = TestResult;

    async fn run(self, mut ctx: Context<'_, impl EmitterFn>) -> anyhow::Result<TestResult> {
        let session = ctx.session(self.sessid);
        tracing::info!("Running test {}", self.test.name);

        // TODO: add an RPC function to create RTT client and return a handle to it
        let rtt_client = create_rtt_client(session, None, &LogOptions::default()).await?;

        run_blocking_streaming(
            ctx,
            move |iface: &mut LocalSession,
                  tx: UnboundedSender<TestEvent>,
                  cancellation_token: CancellationToken|
                  -> anyhow::Result<TestResult> {
                let timeout = self.test.timeout.map(|t| Duration::from_secs(t as u64));
                let timeout = timeout.unwrap_or(Duration::from_secs(60));

                let session = iface.session(self.sessid);

                let mut core = session.core(0)?;
                core.reset_and_halt(Duration::from_millis(100))
                    .map_err(|e| anyhow::anyhow!(e))?;

                let expected_outcome = self.test.expected_outcome;
                let mut run_handler = RunEventHandler::new(self.test);
                let mut rtt_sink =
                    RttStreamer::new(tx.clone(), cancellation_token.clone(), TestEvent::RttOutput);

                let mut run_loop = RunLoop {
                    core_id: 0,
                    rtt_client,
                    cancellation_token,
                };

                match run_loop.run_until(
                    &mut core,
                    true,
                    true,
                    &mut rtt_sink,
                    Some(timeout),
                    |halt_reason, core| run_handler.handle_halt(halt_reason, core),
                )? {
                    ReturnReason::Timeout => Ok(TestResult::Failed(
                        format!("Test timed out after {:?}", timeout),
                        None,
                    )),
                    ReturnReason::Predicate(outcome) => {
                        if outcome == expected_outcome {
                            return Ok(TestResult::Success);
                        }

                        let stack_trace = if outcome == TestOutcome::Panic {
                            let mut stacktrace = String::new();
                            // TODO: pass the path to the binary (or the binary itself)
                            print_stacktrace(&mut core, Path::new(""), &mut stacktrace)?;

                            Some(stacktrace)
                        } else {
                            None
                        };

                        Ok(TestResult::Failed(
                            format!(
                                "Test should {:?} but it did {:?}",
                                expected_outcome, outcome
                            ),
                            stack_trace,
                        ))
                    }
                    ReturnReason::Cancelled => Ok(TestResult::Cancelled),
                }
            },
        )
        .await
    }
}

impl From<RunTest> for RemoteFunctions {
    fn from(func: RunTest) -> Self {
        RemoteFunctions::RunTest(func)
    }
}

struct ListEventHandler {
    semihosting_reader: SemihostingReader,
    cmdline_requested: bool,
}

impl ListEventHandler {
    const SEMIHOSTING_USER_LIST: u32 = 0x100;

    fn new() -> Self {
        Self {
            semihosting_reader: SemihostingReader::new(),
            cmdline_requested: false,
        }
    }

    fn handle_halt(
        &mut self,
        halt_reason: HaltReason,
        core: &mut Core<'_>,
    ) -> anyhow::Result<Option<Tests>> {
        let HaltReason::Breakpoint(BreakpointCause::Semihosting(cmd)) = halt_reason else {
            anyhow::bail!("CPU halted unexpectedly.");
        };

        // When the target first invokes SYS_GET_CMDLINE (0x15), we answer "list"
        // Then, we wait until the target invokes SEMIHOSTING_USER_LIST (0x100) with the json containing all tests
        match cmd {
            SemihostingCommand::GetCommandLine(request) if !self.cmdline_requested => {
                tracing::debug!("target asked for cmdline. send 'list'");
                self.cmdline_requested = true;
                request.write_command_line_to_target(core, "list")?;
                Ok(None) // Continue running
            }
            SemihostingCommand::Unknown(details)
                if details.operation == Self::SEMIHOSTING_USER_LIST && self.cmdline_requested =>
            {
                let buf = details.get_buffer(core)?;
                let buf = buf.read(core)?;
                let list = serde_json::from_slice::<Tests>(&buf[..])?;

                // Signal status=success back to the target
                details.write_status(core, 0)?;

                tracing::debug!("got list of tests from target: {list:?}");
                if list.version != 1 {
                    anyhow::bail!("Unsupported test list format version: {}", list.version);
                }

                Ok(Some(list))
            }
            other @ (SemihostingCommand::Open(_)
            | SemihostingCommand::Close(_)
            | SemihostingCommand::WriteConsole(_)
            | SemihostingCommand::Write(_)) => {
                self.semihosting_reader.handle(other, core)?;
                Ok(None)
            }
            other => anyhow::bail!(
                "Unexpected semihosting command {:?} cmdline_requested: {:?}",
                other,
                self.cmdline_requested
            ),
        }
    }
}

struct RunEventHandler {
    semihosting_reader: SemihostingReader,
    cmdline_requested: bool,
    test: Test,
}

impl RunEventHandler {
    fn new(test: Test) -> Self {
        Self {
            test,
            semihosting_reader: SemihostingReader::new(),
            cmdline_requested: false,
        }
    }

    fn handle_halt(
        &mut self,
        halt_reason: HaltReason,
        core: &mut Core<'_>,
    ) -> anyhow::Result<Option<TestOutcome>> {
        let cmd = match halt_reason {
            HaltReason::Breakpoint(BreakpointCause::Semihosting(cmd)) => cmd,
            e => {
                // Exception occurred (e.g. hardfault) => Abort testing altogether
                anyhow::bail!("The CPU halted unexpectedly: {:?}. Test should signal failure via a panic handler that calls `semihosting::proces::abort()` instead", e)
            }
        };

        match cmd {
            SemihostingCommand::GetCommandLine(request) if !self.cmdline_requested => {
                let cmdline = format!("run {}", self.test.name);
                tracing::debug!("target asked for cmdline. send '{cmdline}'");
                self.cmdline_requested = true;
                request.write_command_line_to_target(core, &cmdline)?;
                Ok(None) // Continue running
            }
            SemihostingCommand::ExitSuccess if self.cmdline_requested => {
                Ok(Some(TestOutcome::Pass))
            }

            SemihostingCommand::ExitError(_) if self.cmdline_requested => {
                Ok(Some(TestOutcome::Panic))
            }
            other @ (SemihostingCommand::Open(_)
            | SemihostingCommand::Close(_)
            | SemihostingCommand::WriteConsole(_)
            | SemihostingCommand::Write(_)) => {
                self.semihosting_reader.handle(other, core)?;
                Ok(None)
            }
            other => {
                // Invalid sequence of semihosting calls => Abort testing altogether
                anyhow::bail!(
                    "Unexpected semihosting command {:?} cmdline_requested: {:?}",
                    other,
                    self.cmdline_requested
                );
            }
        }
    }
}
