//! Backend abstraction for the DAP server.
//!
//! The DAP server historically operated directly against a local
//! [`probe_rs::Session`]. To allow the same debugger implementation to drive a
//! target via the probe-rs RPC layer (over a network connection), the
//! session-level operations the debugger actually needs are captured here in
//! the [`DapBackend`] trait.
//!
//! Two implementations are provided:
//!
//! * [`probe_rs::Session`] (local, blanket impl below).
//! * [`rpc::RpcBackend`], which forwards every operation to a probe-rs RPC
//!   server through a [`crate::rpc::client::RpcClient`].

pub mod rpc;

use std::path::Path;
use std::time::Duration;

use probe_rs::{
    Architecture, Core, CoreInformation, CoreInterface, CoreStatus, CoreType, Error, MemoryInterface,
    RegisterId, RegisterValue, Session, Target, flashing::FlashError,
};
use probe_rs_debug::{
    DebugError, DebugInfo, DebugRegisters, StackFrame, SteppingMode, exception_handler_for_core,
};
use tokio::runtime::Handle;

use crate::cmd::dap_server::DebuggerError;
use crate::cmd::dap_server::debug_adapter::dap::dap_types::{
    EvaluateArguments, EvaluateResponseBody, Scope, Variable,
};
use crate::cmd::dap_server::server::configuration::FlashingConfig;
use crate::rpc::functions::flash::ProgressEvent as WireProgressEvent;
use crate::util::rtt::DataFormat;

/// UI event produced by semihosting handling, to be replayed on the DAP
/// adapter (RTT window open, console/RTT output). Backend-agnostic mirror of
/// the RPC `WireSemihostingUiEvent` so the trait method is available without
/// the `remote` feature.
#[derive(Debug, Clone)]
pub enum SemihostingUiEvent {
    RttWindow {
        handle: u32,
        path: String,
        format: DataFormat,
    },
    LogToConsole(String),
    RttOutput { handle: u32, data: String },
}

/// Result of [`DapBackend::handle_semihosting`]: the post-handling core
/// status and the UI events the caller must replay on the DAP adapter.
#[derive(Debug, Clone)]
pub struct SemihostingHandleResult {
    pub status: CoreStatus,
    pub events: Vec<SemihostingUiEvent>,
}

/// Sync↔async bridge: drive `fut` to completion on `handle` without
/// blocking the runtime (via `block_in_place`).
pub(crate) fn block_on<F: std::future::Future>(handle: &Handle, fut: F) -> F::Output {
    tokio::task::block_in_place(|| handle.block_on(fut))
}

/// Lossy chunked byte read against a [`probe_rs::Core`]: reads as much as
/// possible, stopping at the first unreadable region. Mirrors the historical
/// `CoreHandle::read_memory_lossy` so the local `Session` backend path
/// preserves partial-read behavior.
fn read_memory_lossy(
    core: &mut Core<'_>,
    mut address: u64,
    count: usize,
) -> Result<Vec<u8>, Error> {
    fn chunk_size(count: usize, max_chunk_size: usize) -> usize {
        (max_chunk_size.min(count) / 2).next_power_of_two()
    }

    let mut num_bytes_unread = count;
    let mut result_buffer: Vec<u8> = Vec::new();
    let mut fast_buff = [0u8; 256];
    let mut max_chunk_size = fast_buff.len();

    while num_bytes_unread > 0 && max_chunk_size > 0 {
        let chunk_size = chunk_size(num_bytes_unread, max_chunk_size);
        let buffer = &mut fast_buff[..chunk_size];
        match core.read(address, buffer) {
            Err(e) => {
                if result_buffer.is_empty() && chunk_size == 1 {
                    return Err(e);
                }
                max_chunk_size = chunk_size / 2;
            }
            Ok(()) => {
                result_buffer.extend_from_slice(buffer);
                address += chunk_size as u64;
                num_bytes_unread -= chunk_size;
            }
        }
    }

    Ok(result_buffer)
}

/// Local semihosting handler: performs the file I/O against the live `core`
/// and mutates the client-owned `state`, pushing UI `events` for the caller
/// to replay on the DAP adapter. Mirrors the historical
/// `CoreHandle::handle_semihosting` body.
fn handle_semihosting_local(
    core: &mut Core<'_>,
    state: &mut crate::cmd::dap_server::server::core_data::ClientSemihostingState,
    command: probe_rs::semihosting::SemihostingCommand,
    events: &mut Vec<SemihostingUiEvent>,
) -> Result<CoreStatus, Error> {
    use std::num::NonZeroU32;
    use probe_rs::{BreakpointCause, HaltReason};
    use probe_rs::semihosting::SemihostingCommand;
    use crate::cmd::dap_server::server::core_data::SemihostingFile;

    match command {
        SemihostingCommand::Open(request) => {
            tracing::debug!("Semihosting request: open {request:?}");
            let path = request.path(core)?;
            let mode = request.mode();

            let is_write = mode.starts_with('w') || mode.starts_with('a');
            let is_append = mode.starts_with('a');
            let is_stdio = path == ":tt";

            let path = if is_stdio {
                if is_append { "stderr" } else { "stdout" }.to_string()
            } else {
                path
            };

            let is_binary = mode.ends_with('b');
            let format = if is_binary {
                DataFormat::BinaryLE
            } else {
                DataFormat::String
            };

            if is_write {
                if let Some(file) = state.handles.values().find(|f| f.path == path) {
                    request.respond_with_handle(core, file.handle)?;
                } else {
                    let handle = state.next_handle;
                    #[expect(clippy::unwrap_used, reason = "Infallible from 1024")]
                    let nz_handle = NonZeroU32::new(handle).unwrap();
                    state.handles.insert(
                        handle,
                        SemihostingFile {
                            handle: nz_handle,
                            path: path.clone(),
                            mode,
                        },
                    );
                    state.next_handle += 1;

                    events.push(SemihostingUiEvent::RttWindow {
                        handle,
                        path,
                        format,
                    });
                    request.respond_with_handle(core, nz_handle)?;
                }
            }
        }
        SemihostingCommand::Close(request) => {
            tracing::debug!("Semihosting request: close {request:?}");
            request.success(core)?;
        }
        SemihostingCommand::WriteConsole(request) => {
            tracing::debug!("Semihosting request: write console {request:?}");
            let string = request.read(core)?;
            events.push(SemihostingUiEvent::LogToConsole(string));
        }
        SemihostingCommand::Write(request) => {
            tracing::debug!("Semihosting request: write {request:?}");
            let handle = request.file_handle();
            let bytes = request.read(core)?;

            if let Some(file) = state.handles.get(&handle) {
                let data = if file.mode.ends_with('b') {
                    let mut string = String::new();
                    for byte in bytes {
                        if !string.is_empty() {
                            string.push(' ');
                        }
                        string.push_str(&format!("{byte:02x}"));
                    }
                    string
                } else {
                    String::from_utf8_lossy(&bytes).to_string()
                };

                events.push(SemihostingUiEvent::RttOutput { handle, data });
                request.write_status(core, 0)?;
            }
        }
        SemihostingCommand::Errno(request) => {
            request.write_errno(core, 0)?;
        }

        SemihostingCommand::ExitSuccess => {
            events.push(SemihostingUiEvent::LogToConsole(
                "Application has exited with success.".to_string(),
            ));
            return Ok(CoreStatus::Halted(HaltReason::Breakpoint(
                BreakpointCause::Semihosting(SemihostingCommand::ExitSuccess),
            )));
        }
        SemihostingCommand::ExitError(details) => {
            events.push(SemihostingUiEvent::LogToConsole(format!(
                "Application has exited with {details}"
            )));
            return Ok(CoreStatus::Halted(HaltReason::Breakpoint(
                BreakpointCause::Semihosting(SemihostingCommand::ExitError(details)),
            )));
        }

        unhandled => {
            tracing::warn!("Unhandled semihosting command: {:?}", unhandled);
            return Ok(CoreStatus::Halted(HaltReason::Breakpoint(
                BreakpointCause::Semihosting(unhandled),
            )));
        }
    };

    core.run()?;
    Ok(CoreStatus::Running)
}

/// Seed for driving the server-side RTT client over RPC. Only the RPC
/// backend returns `Some` from [`DapBackend::rtt_remote_seed`]; the local
/// [`Session`] backend returns `None` and uses a local `RttClient` instead.
#[derive(Clone)]
pub struct RttRemoteSeed {
    pub session: crate::rpc::client::SessionInterface,
}
use crate::util::flash::build_loader;

/// Session-level operations used by the DAP server.
///
/// Anything the DAP server needs to do against a "whole target" (as opposed to
/// a single [`Core`]) goes through this trait. The DAP code is written against
/// `SessionData<B: DapBackend>` so it can run against either a local
/// [`Session`] or a remote RPC-backed session implementation.
pub trait DapBackend {
    /// Return the available cores on this target.
    fn list_cores(&self) -> Vec<(usize, CoreType)>;

    /// Return the target description.
    fn target(&self) -> &Target;

    /// Return a handle to the requested core.
    fn core(&mut self, core_index: usize) -> Result<Core<'_>, Error>;

    /// Read the current [`CoreStatus`] of `core_index`. `async` so the RPC
    /// backend can `.await` the round trip directly.
    async fn status(&mut self, core_index: usize) -> Result<CoreStatus, Error> {
        let mut core = self.core(core_index)?;
        core.status()
    }

    /// Set hardware breakpoints at all `addresses` in one round trip. Returns
    /// per-address success so callers can preserve per-breakpoint DAP
    /// verification feedback. Default impl loops via [`DapBackend::core`];
    /// the RPC backend overrides this to issue a single `core/set_hw_bps`
    /// round trip.
    async fn set_hw_breakpoints(
        &mut self,
        core_index: usize,
        addresses: Vec<u64>,
    ) -> Result<Vec<bool>, Error> {
        let mut core = self.core(core_index)?;
        let mut out = Vec::with_capacity(addresses.len());
        for address in addresses {
            out.push(core.set_hw_breakpoint(address).is_ok());
        }
        Ok(out)
    }

    /// Clear hardware breakpoints at all `addresses` in one round trip.
    async fn clear_hw_breakpoints(
        &mut self,
        core_index: usize,
        addresses: Vec<u64>,
    ) -> Result<(), Error> {
        let mut core = self.core(core_index)?;
        for address in addresses {
            match core.clear_hw_breakpoint(address) {
                Ok(()) => {}
                Err(Error::BreakpointOperation(probe_rs::BreakpointError::NotFound(_))) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Lossy bulk byte read: returns as many bytes as are readable starting
    /// at `address`, stopping at the first unreadable region. Default impl
    /// loops via [`DapBackend::core`]; the RPC backend overrides this to
    /// issue a single `memory/read_bytes` round trip.
    async fn read_memory(
        &mut self,
        core_index: usize,
        address: u64,
        count: usize,
    ) -> Result<Vec<u8>, Error> {
        let mut core = self.core(core_index)?;
        read_memory_lossy(&mut core, address, count)
    }

    /// Write `data` to `address`. Default impl uses [`DapBackend::core`];
    /// the RPC backend overrides this to issue a single `memory/write8`
    /// round trip.
    async fn write_memory(
        &mut self,
        core_index: usize,
        address: u64,
        data: Vec<u8>,
    ) -> Result<(), Error> {
        let mut core = self.core(core_index)?;
        core.write_8(address, &data)
    }

    async fn halt(
        &mut self,
        core_index: usize,
        timeout: Duration,
    ) -> Result<CoreInformation, Error> {
        let mut core = self.core(core_index)?;
        core.halt(timeout)
    }

    async fn run(&mut self, core_index: usize) -> Result<(), Error> {
        let mut core = self.core(core_index)?;
        core.run()
    }

    /// Reset and halt the core, returning [`CoreInformation`]. Default via
    /// [`DapBackend::core`]; the RPC backend overrides this to `.await` the
    /// `reset_and_halt` round trip (which returns the wire `CoreInformation`).
    async fn reset_and_halt(
        &mut self,
        core_index: usize,
        timeout: Duration,
    ) -> Result<CoreInformation, Error> {
        let mut core = self.core(core_index)?;
        core.reset_and_halt(timeout)
    }

    /// Whether the core is halted. Default via [`DapBackend::core`]; the RPC
    /// backend overrides this to `.await` the `core/halted` round trip.
    async fn core_halted(&mut self, core_index: usize) -> Result<bool, Error> {
        let mut core = self.core(core_index)?;
        core.core_halted()
    }

    /// Static core architecture. The default impl queries the live `Core`;
    /// the RPC backend overrides this to read its cached per-core metadata
    /// (no round trip), so callers can branch on architecture without a
    /// `Core` (e.g. `reapply_breakpoints` after reset).
    async fn core_architecture(&mut self, core_index: usize) -> Result<Architecture, Error> {
        let core = self.core(core_index)?;
        Ok(core.architecture())
    }

    /// Static `RegisterId` of the program counter. Default via
    /// [`DapBackend::core`]; the RPC backend overrides this to read the
    /// cached per-core register set (no round trip), so callers can read the
    /// PC via [`DapBackend::read_core_reg`] without a `Core`.
    async fn program_counter_id(&mut self, core_index: usize) -> Result<RegisterId, Error> {
        let core = self.core(core_index)?;
        Ok(core.program_counter().id)
    }

    /// Set a single hardware breakpoint. Default via [`DapBackend::core`];
    /// the RPC backend overrides this to `.await` the `core/set_hw_bp` round
    /// trip. (Batched set is [`DapBackend::set_hw_breakpoints`].)
    async fn set_hw_breakpoint(&mut self, core_index: usize, address: u64) -> Result<(), Error> {
        let mut core = self.core(core_index)?;
        core.set_hw_breakpoint(address)
    }

    /// Clear a single hardware breakpoint. Returns whether a cached
    /// breakpoint entry was removed. Default via [`DapBackend::core`]; the
    /// RPC backend overrides this to `.await` the `core/clear_hw_bp` round
    /// trip. (Batched clear is [`DapBackend::clear_hw_breakpoints`].)
    async fn clear_hw_breakpoint(&mut self, core_index: usize, address: u64) -> Result<(), Error> {
        let mut core = self.core(core_index)?;
        match core.clear_hw_breakpoint(address) {
            Ok(()) => Ok(()),
            Err(Error::BreakpointOperation(probe_rs::BreakpointError::NotFound(_))) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Set a local/static variable's value. Default impl is a no-op error:
    /// the local `Session` path resolves variables against the
    /// client-side `VariableCache` (in `CoreData`) directly in the
    /// `set_variable` handler, so this method is only reached by the RPC
    /// backend, whose override `.await`s the `stack_trace/set_variable`
    /// round trip (the `VariableCache` lives server-side).
    async fn set_variable(
        &mut self,
        _core_index: usize,
        _parent_key: i64,
        _name: String,
        _value: String,
    ) -> Result<crate::rpc::functions::debug_vars::WireSetVariableResponse, Error> {
        Err(Error::Other(
            "Variable not found in any client-side cache.".to_string(),
        ))
    }

    /// Disassemble target memory. Default impl runs the shared
    /// `disassemble_target_memory` against a local `Core` + the supplied
    /// `DebugInfo`; the RPC backend overrides this to `.await` the
    /// `core/disassemble` round trip (the server owns the `DebugInfo` and
    /// does the capstone work, so the `debug_info` arg is ignored).
    async fn disassemble(
        &mut self,
        core_index: usize,
        debug_info: Option<&probe_rs_debug::DebugInfo>,
        memory_reference: u64,
        byte_offset: i64,
        instruction_offset: i64,
        instruction_count: i64,
    ) -> Result<
        Vec<crate::cmd::dap_server::debug_adapter::dap::dap_types::DisassembledInstruction>,
        Error,
    > {
        use crate::cmd::dap_server::debug_adapter::dap::request_helpers::{
            DisassemblyAmount, disassemble_target_memory,
        };
        let mut core = self.core(core_index)?;
        let instruction_set = core.instruction_set()?;
        let core_type = core.core_type();
        let endianness = core.endianness()?;
        disassemble_target_memory(
            &mut core,
            instruction_set,
            core_type,
            endianness,
            debug_info,
            instruction_offset,
            byte_offset,
            memory_reference,
            DisassemblyAmount::Instructions(instruction_count),
        )
        .map_err(|e| Error::Other(e.to_string()))
    }

    /// Read a single core register. Default via [`DapBackend::core`]; the RPC
    /// backend overrides this to `.await` the `core/read_reg` round trip.
    async fn read_core_reg(
        &mut self,
        core_index: usize,
        register_id: RegisterId,
    ) -> Result<RegisterValue, Error> {
        let mut core = self.core(core_index)?;
        core.read_core_reg(register_id)
    }

    /// Read a batch of core registers in one round trip (RPC) or a tight loop
    /// (local). Per-register failures are reported as `None` in-place.
    async fn read_core_registers(
        &mut self,
        core_index: usize,
        ids: Vec<RegisterId>,
    ) -> Result<Vec<Option<RegisterValue>>, Error> {
        let mut core = self.core(core_index)?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(core.read_core_reg::<RegisterValue>(id).ok());
        }
        Ok(out)
    }

    /// The static register file for this core (names, roles, sizes, ids).
    /// Both backends can produce this without a round trip: the local backend
    /// reads it from the live `Core`, the RPC backend from cached per-core
    /// metadata.
    fn register_file(
        &mut self,
        core_index: usize,
    ) -> Result<&'static probe_rs::CoreRegisters, Error> {
        let core = self.core(core_index)?;
        Ok(core.registers())
    }

    /// Read `count` bytes of target memory at `address` into a fresh buffer.
    async fn read_memory_8(
        &mut self,
        core_index: usize,
        address: u64,
        count: usize,
    ) -> Result<Vec<u8>, Error> {
        let mut core = self.core(core_index)?;
        let mut buf = vec![0u8; count];
        core.read_8(address, &mut buf)?;
        Ok(buf)
    }

    /// Dump the core (all registers + the supplied memory ranges) to a
    /// [`probe_rs::CoreDump`]. Default via [`DapBackend::core`]; the RPC
    /// backend overrides this to `.await` a `core/dump` round trip and
    /// reconstruct the `CoreDump` client-side.
    async fn dump_core(
        &mut self,
        core_index: usize,
        ranges: Vec<std::ops::Range<u64>>,
    ) -> Result<probe_rs::CoreDump, Error> {
        let mut core = self.core(core_index)?;
        probe_rs::CoreDump::dump_core(&mut core, ranges)
    }

    /// Handle a semihosting halt: perform the file I/O (open/read/write) and
    /// return the resulting [`CoreStatus`] plus the UI events the caller must
    /// replay on the DAP adapter. The default (local) impl drives the live
    /// [`probe_rs::Core`] via [`DapBackend::core`] and mutates the supplied
    /// [`ClientSemihostingState`]; the RPC backend overrides this to `.await`
    /// the `core/handle_semihosting` round trip (the state is server-owned
    /// and the `state` argument is unused).
    async fn handle_semihosting(
        &mut self,
        core_index: usize,
        state: &mut crate::cmd::dap_server::server::core_data::ClientSemihostingState,
    ) -> Result<SemihostingHandleResult, Error> {
        use probe_rs::{BreakpointCause, HaltReason};

        let mut core = self.core(core_index)?;
        let status = core.status()?;
        let Some(command) = (match status {
            CoreStatus::Halted(HaltReason::Breakpoint(BreakpointCause::Semihosting(c))) => Some(c),
            _ => None,
        }) else {
            return Ok(SemihostingHandleResult {
                status,
                events: vec![],
            });
        };

        let mut events = Vec::new();
        let result = handle_semihosting_local(&mut core, state, command, &mut events)?;
        Ok(SemihostingHandleResult {
            status: result,
            events,
        })
    }

    /// Kick off a single embedded-test case (DAP REPL `test run`): run until
    /// the `GetCommandLine` semihosting call, write `run_addr {address}` as
    /// the command line, then resume. Default via [`DapBackend::core`]; the
    /// RPC backend overrides this to `.await` the `tests/kickoff` round trip.
    async fn kickoff_test(&mut self, core_index: usize, address: u64) -> Result<(), Error> {
        use probe_rs::{BreakpointCause, CoreStatus, HaltReason};
        use probe_rs::semihosting::SemihostingCommand;

        let mut core = self.core(core_index)?;
        core.run()?;
        core.wait_for_core_halted(Duration::from_secs(1))?;
        let CoreStatus::Halted(HaltReason::Breakpoint(BreakpointCause::Semihosting(
            SemihostingCommand::GetCommandLine(cmd),
        ))) = core.status()?
        else {
            return Err(Error::Other(
                "Could not start test: target did not halt on GetCommandLine".to_string(),
            ));
        };
        cmd.write_command_line_to_target(&mut core, &format!("run_addr {address}"))?;
        core.run()?;
        Ok(())
    }

    /// Write a single core register. Default via [`DapBackend::core`]; the
    /// RPC backend overrides this to `.await` the `core/write_reg` round trip.
    async fn write_core_reg(
        &mut self,
        core_index: usize,
        register_id: RegisterId,
        value: RegisterValue,
    ) -> Result<(), Error> {
        let mut core = self.core(core_index)?;
        core.write_core_reg(register_id, value)
    }

    /// Full `SteppingMode::step` (over/into/out/instruction) run against the
    /// live `Core` with the supplied `DebugInfo`. Returns the new
    /// [`CoreStatus`], program counter, and any `WarnAndContinue` message.
    /// Default impl runs `SteppingMode::step` locally via
    /// [`DapBackend::core`]; the RPC backend overrides this to `.await` the
    /// `stack_trace/step` round trip (the server owns the cached `DebugInfo`).
    async fn debug_step(
        &mut self,
        core_index: usize,
        mode: SteppingMode,
        debug_info: Option<&DebugInfo>,
    ) -> Result<(CoreStatus, u64, Option<String>), Error> {
        let mut core = self.core(core_index)?;
        match mode.step(&mut core, debug_info) {
            Ok((status, pc)) => Ok((status, pc, None)),
            Err(DebugError::WarnAndContinue { message }) => {
                let status = core.status()?;
                let pc: u64 = core
                    .read_core_reg::<RegisterValue>(core.program_counter().id())?
                    .try_into()?;
                Ok((status, pc, Some(message)))
            }
            Err(other) => {
                core.halt(Duration::from_millis(100)).ok();
                Err(Error::Other(other.to_string()))
            }
        }
    }

    /// If `Some`, drive the server-side RTT client over RPC (RPC backend);
    /// if `None`, use a local `RttClient` (local `Session` backend).
    fn rtt_remote_seed(&self) -> Option<RttRemoteSeed> {
        None
    }

    /// Build the stack frames for `core_index` while it is halted. The
    /// default unwinds locally via [`DapBackend::core`]; the RPC backend
    /// overrides this to issue a single `stack_trace/rich` round trip and
    /// rebuild locals from the supplied `debug_info`.
    async fn unwind_stack(
        &mut self,
        core_index: usize,
        _program_binary: Option<&Path>,
        debug_info: &DebugInfo,
        max_frames: usize,
    ) -> Result<Vec<StackFrame>, Error> {
        let mut core = self.core(core_index)?;
        let initial_registers = DebugRegisters::from_core(&mut core);
        let exception_interface = exception_handler_for_core(core.core_type());
        let instruction_set = core.instruction_set().ok();
        debug_info.unwind(
            &mut core,
            initial_registers,
            exception_interface.as_ref(),
            instruction_set,
            max_frames,
        )
    }

    /// Resolve DAP `scopes` for `frame_id` server-side. The RPC backend
    /// overrides this to issue a single `stack_trace/scopes` round trip
    /// against its cached `VariableCache`; the local `Session` backend
    /// returns `Ok(None)` so the existing client-side `scopes` logic runs.
    async fn scopes(
        &mut self,
        _core_index: usize,
        _frame_id: u32,
    ) -> Result<Option<Vec<Scope>>, Error> {
        Ok(None)
    }

    /// Resolve DAP `variables` for `variables_reference` server-side. The
    /// RPC backend overrides this to issue a single `stack_trace/variables`
    /// round trip (with lazy expansion) against its cached `VariableCache`;
    /// the local `Session` backend returns `Ok(None)`.
    async fn variables(
        &mut self,
        _core_index: usize,
        _variables_reference: u32,
        _filter: Option<String>,
    ) -> Result<Option<Vec<Variable>>, Error> {
        Ok(None)
    }

    /// Evaluate a watch/hover expression server-side. The RPC backend
    /// overrides this to issue a single `stack_trace/evaluate` round trip
    /// against its cached `VariableCache`; the local `Session` backend
    /// returns `Ok(None)` so the existing client-side `evaluate` logic runs.
    /// Only the `watch`/`hover` contexts are handled server-side; `repl` and
    /// `clipboard` always fall back to the local path.
    async fn evaluate(
        &mut self,
        _core_index: usize,
        _arguments: &EvaluateArguments,
    ) -> Result<Option<EvaluateResponseBody>, Error> {
        Ok(None)
    }
}

impl DapBackend for Session {
    fn list_cores(&self) -> Vec<(usize, CoreType)> {
        Session::list_cores(self)
    }

    fn target(&self) -> &Target {
        Session::target(self)
    }

    fn core(&mut self, core_index: usize) -> Result<Core<'_>, Error> {
        Session::core(self, core_index)
    }
}

/// Extension trait used by the DAP server to flash a binary during `launch`
/// and `restart` handling.
///
/// A dedicated trait allows the [`Session`] path to run the historical
/// synchronous flash while the RPC path issues the build/verify/flash
/// operations over the wire. Progress events are surfaced as the wire-format
/// [`WireProgressEvent`] so the DAP server renders progress uniformly.
pub trait FlashingBackend: DapBackend {
    /// Flash `path_to_elf` to the target, invoking `progress` for every
    /// progress event emitted along the way.
    ///
    /// Implementations MUST respect
    /// [`FlashingConfig::verify_before_flashing`]/
    /// [`FlashingConfig::verify_after_flashing`]/
    /// [`FlashingConfig::restore_unwritten_bytes`]/
    /// [`FlashingConfig::full_chip_erase`].
    async fn flash_binary(
        &mut self,
        path_to_elf: &Path,
        config: &FlashingConfig,
        progress: &mut dyn FnMut(WireProgressEvent),
    ) -> Result<(), DebuggerError>;
}

impl FlashingBackend for Session {
    async fn flash_binary(
        &mut self,
        path_to_elf: &Path,
        config: &FlashingConfig,
        progress: &mut dyn FnMut(WireProgressEvent),
    ) -> Result<(), DebuggerError> {
        use probe_rs::flashing::{DownloadOptions, FileDownloadError, FlashProgress};

        let loader = build_loader(self, path_to_elf, config.format_options.clone(), None)?;

        let mut download_options = DownloadOptions::default();
        download_options.keep_unwritten_bytes = config.restore_unwritten_bytes;
        download_options.do_chip_erase = config.full_chip_erase;
        download_options.verify = config.verify_after_flashing;
        // `FlashProgress` carries a borrow (its lifetime parameter `'a`) so
        // we can pass the caller-provided `&mut dyn FnMut` through without
        // any unsafe.
        download_options.progress = FlashProgress::new(|event| {
            WireProgressEvent::from_library_event(event, &mut *progress);
        });

        let do_flashing = if config.verify_before_flashing {
            match loader.verify(self, &mut download_options.progress) {
                Ok(_) => false,
                Err(FlashError::Verify) => true,
                Err(other) => {
                    return Err(DebuggerError::FileDownload(FileDownloadError::Flash(other)));
                }
            }
        } else {
            true
        };

        if do_flashing {
            loader
                .commit(self, download_options)
                .map_err(FileDownloadError::Flash)
                .map_err(DebuggerError::FileDownload)?;
        }

        Ok(())
    }
}
