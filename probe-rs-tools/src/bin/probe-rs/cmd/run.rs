use tokio_util::sync::CancellationToken;

use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use probe_rs::{
    rtt::Error as RttError, Core, CoreInterface, Error, HaltReason, VectorCatchCondition,
};
use probe_rs_debug::{exception_handler_for_core, DebugInfo, DebugRegisters};

use crate::cmd::remote::functions::monitor::{LogOptions, MonitorMode, MonitorOptions};
use crate::cmd::remote::ClientInterface;
use crate::util::cli;
use crate::util::common_options::{BinaryDownloadOptions, ProbeOptions};
use crate::util::rtt::client::RttClient;
use crate::util::rtt::ChannelDataCallbacks;
use crate::FormatOptions;

use libtest_mimic::{Arguments, FormatSetting};
use probe_rs::flashing::FileDownloadError;
use std::fs::File;
use std::io::Read;

/// Options only used in normal run mode
#[derive(Debug, clap::Parser, Clone)]
pub struct NormalRunOptions {
    /// Enable reset vector catch if its supported on the target.
    #[clap(long, help_heading = "RUN OPTIONS")]
    pub catch_reset: bool,
    /// Enable hardfault vector catch if its supported on the target.
    #[clap(long, help_heading = "RUN OPTIONS")]
    pub catch_hardfault: bool,
}

/// Options only used when in test run mode
#[derive(Debug, clap::Parser)]
pub struct TestOptions {
    /// Filter string. Only tests which contain this string are run.
    #[clap(
        index = 2,
        value_name = "TEST_FILTER",
        help = "The TEST_FILTER string is tested against the name of all tests, and only those tests whose names contain the filter are run. Multiple filter strings may be passed, which will run all tests matching any of the filters.",
        help_heading = "TEST OPTIONS"
    )]
    pub filter: Vec<String>,

    /// Only list all tests
    #[clap(
        long = "list",
        help = "List all tests instead of executing them",
        help_heading = "TEST OPTIONS"
    )]
    pub list: bool,

    #[clap(
        long = "format",
        value_enum,
        value_name = "pretty|terse|json",
        help_heading = "TEST OPTIONS",
        help = "Configure formatting of the test report output"
    )]
    pub format: Option<FormatSetting>,

    /// If set, filters are matched exactly rather than by substring.
    #[clap(long = "exact", help_heading = "TEST OPTIONS")]
    pub exact: bool,

    /// If set, run only ignored tests.
    #[clap(long = "ignored", help_heading = "TEST OPTIONS")]
    pub ignored: bool,

    /// If set, run ignored and non-ignored tests.
    #[clap(long = "include-ignored", help_heading = "TEST OPTIONS")]
    pub include_ignored: bool,

    /// A list of filters. Tests whose names contain parts of any of these
    /// filters are skipped.
    #[clap(
        long = "skip-test",
        value_name = "FILTER",
        help_heading = "TEST OPTIONS",
        help = "Skip tests whose names contain FILTER (this flag can be used multiple times)"
    )]
    pub skip_test: Vec<String>,

    /// Options which are ignored, but exist for compatibility with libtest.
    /// E.g. so that vscode and intellij can invoke the test runner with the args they are used to
    #[clap(flatten)]
    _no_op: NoOpTestOptions,
}

/// Options which are ignored, but exist for compatibility with libtest.
#[derive(Debug, clap::Parser)]
struct NoOpTestOptions {
    // No-op, ignored (libtest-mimic always runs in no-capture mode)
    #[clap(long = "nocapture", hide = true)]
    nocapture: bool,

    /// No-op, ignored. libtest-mimic does not currently capture stdout.
    #[clap(long = "show-output", hide = true)]
    show_output: bool,

    /// No-op, ignored. Flag only exists for CLI compatibility with libtest.
    #[clap(short = 'Z', hide = true)]
    unstable_flags: Option<String>,
}

#[derive(clap::Parser)]
pub struct Cmd {
    /// Options only used when in normal run mode
    #[clap(flatten)]
    pub(crate) run_options: NormalRunOptions,

    /// Options only used when in test mode
    #[clap(flatten)]
    pub(crate) test_options: TestOptions,

    /// Options shared by all run modes
    #[clap(flatten)]
    pub(crate) shared_options: SharedOptions,
}

#[derive(Debug, clap::Parser)]
pub struct SharedOptions {
    #[clap(flatten)]
    pub(crate) probe_options: ProbeOptions,

    #[clap(flatten)]
    pub(crate) download_options: BinaryDownloadOptions,

    /// The path to the ELF file to flash and run.
    #[clap(
        index = 1,
        help = "The path to the ELF file to flash and run.\n\
    If the binary uses `embedded-test` each test will be executed in turn. See `TEST OPTIONS` for more configuration options exclusive to this mode.\n\
    If the binary does not use `embedded-test` the binary will be flashed and run normally. See `RUN OPTIONS` for more configuration options exclusive to this mode."
    )]
    pub(crate) path: PathBuf,

    /// Always print the stacktrace on ctrl + c.
    #[clap(long)]
    pub(crate) always_print_stacktrace: bool,

    /// Whether to erase the entire chip before downloading
    #[clap(long, help_heading = "DOWNLOAD CONFIGURATION")]
    pub(crate) chip_erase: bool,

    /// Suppress filename and line number information from the rtt log
    #[clap(long)]
    pub(crate) no_location: bool,

    #[clap(flatten)]
    pub(crate) format_options: FormatOptions,

    /// The format string to use when printing defmt encoded log messages from the target.
    ///
    /// See https://defmt.ferrous-systems.com/custom-log-output
    #[clap(long)]
    pub(crate) log_format: Option<String>,

    /// Scan the memory to find the RTT control block
    #[clap(long)]
    pub(crate) rtt_scan_memory: bool,
}

impl Cmd {
    pub async fn run(self, mut iface: impl ClientInterface) -> anyhow::Result<()> {
        // Detect run mode based on ELF file
        let run_mode = detect_run_mode(&self)?;

        let mut session =
            cli::attach_probe(&mut iface, self.shared_options.probe_options, true).await?;

        // Flash firmware
        let flash_result = cli::flash(
            &mut session,
            &self.shared_options.path,
            self.shared_options.chip_erase,
            self.shared_options.format_options,
            self.shared_options.download_options,
        )
        .await?;

        // Run firmware based on run mode
        if run_mode == RunMode::Test {
            let sessid = session.into_session_id();
            cli::test(
                iface,
                sessid,
                flash_result.boot_info,
                Arguments {
                    test_threads: Some(1), // Avoid parallel execution
                    list: self.test_options.list,
                    exact: self.test_options.exact,
                    ignored: self.test_options.ignored,
                    include_ignored: self.test_options.include_ignored,
                    format: self.test_options.format,
                    skip: self.test_options.skip_test.clone(),
                    filter: if self.test_options.filter.is_empty() {
                        None
                    } else {
                        //TODO: Fix libtest-mimic so that it allows multiple filters (same as std test runners)
                        Some(self.test_options.filter.join(" "))
                    },
                    ..Arguments::default()
                },
                self.shared_options.always_print_stacktrace,
                &self.shared_options.path,
            )
            .await
        } else {
            cli::monitor(
                &mut session,
                MonitorMode::Run(flash_result.boot_info),
                &self.shared_options.path,
                MonitorOptions {
                    catch_reset: self.run_options.catch_reset,
                    catch_hardfault: self.run_options.catch_hardfault,
                    log: LogOptions {
                        no_location: self.shared_options.no_location,
                        log_format: self.shared_options.log_format,
                        rtt_scan_memory: self.shared_options.rtt_scan_memory,
                    },
                },
                self.shared_options.always_print_stacktrace,
            )
            .await
        }
    }
}

#[derive(PartialEq)]
enum RunMode {
    Normal,
    Test,
}

fn elf_contains_test(path: &Path) -> anyhow::Result<bool> {
    let mut file = File::open(path).map_err(FileDownloadError::IO)?;

    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;

    let contains = match goblin::elf::Elf::parse(buffer.as_slice()) {
        Ok(elf) if elf.syms.is_empty() => {
            tracing::debug!("No Symbols in ELF");
            false
        }
        Ok(elf) => elf
            .syms
            .iter()
            .any(|sym| elf.strtab.get_at(sym.st_name) == Some("EMBEDDED_TEST_VERSION")),
        Err(_) => {
            tracing::debug!("Failed to parse ELF file");
            false
        }
    };

    Ok(contains)
}

fn detect_run_mode(cmd: &Cmd) -> anyhow::Result<RunMode> {
    if elf_contains_test(&cmd.shared_options.path)? {
        // We tolerate the run options, even in test mode so that you can set
        // `probe-rs run --catch-hardfault` as cargo runner (used for both unit tests and normal binaries)
        tracing::info!("Detected embedded-test in ELF file. Running as test");
        Ok(RunMode::Test)
    } else {
        let test_args_specified = cmd.test_options.list
            || cmd.test_options.exact
            || cmd.test_options.format.is_some()
            || !cmd.test_options.filter.is_empty();

        if test_args_specified {
            anyhow::bail!("probe-rs was invoked with arguments exclusive to test mode, but the binary does not contain embedded-test");
        }

        tracing::debug!("No embedded-test in ELF file. Running as normal");
        Ok(RunMode::Normal)
    }
}

pub struct RunLoop {
    pub core_id: usize,
    pub rtt_client: RttClient,
    pub cancellation_token: CancellationToken,
}

#[derive(PartialEq, Debug)]
pub enum ReturnReason<R> {
    /// The predicate requested a return
    Predicate(R),
    /// Timeout elapsed
    Timeout,
    /// Cancelled
    Cancelled,
}

impl RunLoop {
    /// Attaches to RTT and runs the core until it halts.
    ///
    /// Upon halt the predicate is invoked with the halt reason:
    /// * If the predicate returns `Ok(Some(r))` the run loop returns `Ok(ReturnReason::Predicate(r))`.
    /// * If the predicate returns `Ok(None)` the run loop will continue running the core.
    /// * If the predicate returns `Err(e)` the run loop will return `Err(e)`.
    ///
    /// The function will also return on timeout with `Ok(ReturnReason::Timeout)` or if the user presses CTRL + C with `Ok(ReturnReason::User)`.
    pub fn run_until<F, R>(
        &mut self,
        core: &mut Core,
        catch_hardfault: bool,
        catch_reset: bool,
        output_stream: &mut dyn Write,
        timeout: Option<Duration>,
        mut predicate: F,
    ) -> Result<ReturnReason<R>>
    where
        F: FnMut(HaltReason, &mut Core) -> Result<Option<R>>,
    {
        if catch_hardfault || catch_reset {
            if !core.core_halted()? {
                core.halt(Duration::from_millis(100))?;
            }

            if catch_hardfault {
                match core.enable_vector_catch(VectorCatchCondition::HardFault) {
                    Ok(_) | Err(Error::NotImplemented(_)) => {} // Don't output an error if vector_catch hasn't been implemented
                    Err(e) => tracing::error!("Failed to enable_vector_catch: {:?}", e),
                }
            }
            if catch_reset {
                match core.enable_vector_catch(VectorCatchCondition::CoreReset) {
                    Ok(_) | Err(Error::NotImplemented(_)) => {} // Don't output an error if vector_catch hasn't been implemented
                    Err(e) => tracing::error!("Failed to enable_vector_catch: {:?}", e),
                }
            }
        }

        if core.core_halted()? {
            core.run()?;
        }
        let start = Instant::now();

        let result = self.do_run_until(core, output_stream, timeout, start, &mut predicate);

        // Always clean up after RTT but don't overwrite the original result.
        let cleanup_result = self.rtt_client.clean_up(core);

        if result.is_ok() {
            // If the result is Ok, we return the potential error during cleanup.
            cleanup_result?;
        }

        result
    }

    fn do_run_until<F, R>(
        &mut self,
        core: &mut Core,
        output_stream: &mut dyn Write,
        timeout: Option<Duration>,
        start: Instant,
        predicate: &mut F,
    ) -> Result<ReturnReason<R>>
    where
        F: FnMut(HaltReason, &mut Core) -> Result<Option<R>>,
    {
        loop {
            // check for halt first, poll rtt after.
            // this is important so we do one last poll after halt, so we flush all messages
            // the core printed before halting, such as a panic message.
            let mut return_reason = None;
            let mut was_halted = false;
            match core.status()? {
                probe_rs::CoreStatus::Halted(reason) => match predicate(reason, core) {
                    Ok(Some(r)) => return_reason = Some(Ok(ReturnReason::Predicate(r))),
                    Err(e) => return_reason = Some(Err(e)),
                    Ok(None) => {
                        was_halted = true;
                        core.run()?
                    }
                },
                probe_rs::CoreStatus::Running
                | probe_rs::CoreStatus::Sleeping
                | probe_rs::CoreStatus::Unknown => {
                    // Carry on
                }

                probe_rs::CoreStatus::LockedUp => {
                    return Err(anyhow!("The core is locked up."));
                }
            }

            let had_rtt_data = poll_rtt(&mut self.rtt_client, core, output_stream)?;

            if let Some(reason) = return_reason {
                return reason;
            } else if let Some(timeout) = timeout {
                if start.elapsed() >= timeout {
                    return Ok(ReturnReason::Timeout);
                }
            } else if self.cancellation_token.is_cancelled() {
                return Ok(ReturnReason::Cancelled);
            }

            // Poll RTT with a frequency of 10 Hz if we do not receive any new data.
            // Once we receive new data, we bump the frequency to 1kHz.
            //
            // We also poll at 1kHz if the core was halted, to speed up reading strings
            // from semihosting. The core is not expected to be halted for other reasons.
            //
            // If the polling frequency is too high, the USB connection to the probe
            // can become unstable. Hence we only pull as little as necessary.
            if had_rtt_data || was_halted {
                thread::sleep(Duration::from_millis(1));
            } else {
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Prints the stacktrace of the current execution state.
pub fn print_stacktrace<S: Write + ?Sized>(
    core: &mut impl CoreInterface,
    path: &Path,
    output_stream: &mut S,
) -> Result<(), anyhow::Error> {
    let Some(debug_info) = DebugInfo::from_file(path).ok() else {
        tracing::error!("No debug info found.");
        return Ok(());
    };
    let initial_registers = DebugRegisters::from_core(core);
    let exception_interface = exception_handler_for_core(core.core_type());
    let instruction_set = core.instruction_set().ok();
    let stack_frames = debug_info
        .unwind(
            core,
            initial_registers,
            exception_interface.as_ref(),
            instruction_set,
        )
        .unwrap();
    for (i, frame) in stack_frames.iter().enumerate() {
        write!(
            output_stream,
            "Frame {}: {} @ {}",
            i, frame.function_name, frame.pc
        )?;

        if frame.is_inlined {
            write!(output_stream, " inline")?;
        }
        writeln!(output_stream)?;

        let Some(location) = &frame.source_location else {
            continue;
        };

        write!(output_stream, "       ")?;

        write!(output_stream, "{}", location.path.to_path().display())?;

        if let Some(line) = location.line {
            write!(output_stream, ":{line}")?;

            if let Some(col) = location.column {
                let col = match col {
                    probe_rs_debug::ColumnType::LeftEdge => 1,
                    probe_rs_debug::ColumnType::Column(c) => c,
                };
                write!(output_stream, ":{col}")?;
            }
        }

        writeln!(output_stream)?;
    }
    Ok(())
}

/// Poll RTT and print the received buffer.
fn poll_rtt<S: Write + ?Sized>(
    rtt_client: &mut RttClient,
    core: &mut Core<'_>,
    out_stream: &mut S,
) -> Result<bool, anyhow::Error> {
    struct OutCollector<'a, O: Write + ?Sized> {
        out_stream: &'a mut O,
        had_data: bool,
    }

    impl<O: Write + ?Sized> ChannelDataCallbacks for OutCollector<'_, O> {
        fn on_string_data(&mut self, _channel: usize, data: String) -> Result<(), RttError> {
            if data.is_empty() {
                return Ok(());
            }
            self.had_data = true;
            self.out_stream
                .write_str(&data)
                .map_err(|err| anyhow!(err))?;
            Ok(())
        }
    }

    let mut out = OutCollector {
        out_stream,
        had_data: false,
    };

    rtt_client.poll(core, &mut out)?;

    Ok(out.had_data)
}
