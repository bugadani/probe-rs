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
    Architecture, Core, CoreInformation, CoreStatus, CoreType, Error, MemoryInterface, RegisterId,
    RegisterValue, Session, Target, flashing::FlashError,
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

/// Seed for driving the server-side RTT client over RPC. Only the RPC
/// backend returns `Some` from [`DapBackend::rtt_remote_seed`]; the local
/// [`Session`] backend returns `None` and uses a local `RttClient` instead.
#[derive(Clone)]
pub struct RttRemoteSeed {
    pub handle: Handle,
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

    /// Reset the core (no halt). Default via [`DapBackend::core`]; the RPC
    /// backend overrides this to `.await` the `reset` round trip.
    async fn reset(&mut self, core_index: usize) -> Result<(), Error> {
        let mut core = self.core(core_index)?;
        core.reset()
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

    /// Single-instruction step, returning [`CoreInformation`]. Default via
    /// [`DapBackend::core`]; the RPC backend overrides this to `.await` the
    /// `core/step` round trip. (Full `SteppingMode` stepping is a separate
    /// server-side endpoint — see `stack_trace/step`.)
    async fn step(&mut self, core_index: usize) -> Result<CoreInformation, Error> {
        let mut core = self.core(core_index)?;
        core.step()
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
