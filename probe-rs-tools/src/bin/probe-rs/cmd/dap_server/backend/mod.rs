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

use probe_rs::{Core, CoreType, Error, Session, Target, flashing::FlashError};
use probe_rs_debug::{DebugInfo, DebugRegisters, StackFrame, exception_handler_for_core};
use tokio::runtime::Handle;

use crate::cmd::dap_server::DebuggerError;
use crate::cmd::dap_server::server::configuration::FlashingConfig;
use crate::rpc::functions::flash::ProgressEvent as WireProgressEvent;

/// Run an async future to completion on the current tokio runtime, without
/// actually blocking the runtime (by releasing the worker thread via
/// [`tokio::task::block_in_place`]).
///
/// This is the single sync↔async bridge used by the DAP server (which is
/// synchronous) to drive async RPC calls. It was previously duplicated in
/// `backend/rpc.rs`, `server/debug_rtt.rs` and `server/core_data.rs`.
pub(crate) fn block_on<F: std::future::Future>(handle: &Handle, fut: F) -> F::Output {
    tokio::task::block_in_place(|| handle.block_on(fut))
}

/// Seed for driving the **server-side** RTT client over RPC.
///
/// Only the RPC backend returns `Some` from [`DapBackend::rtt_remote_seed`];
/// the local [`Session`] backend returns `None`, in which case the DAP server
/// falls back to a local [`RttClient`] that reads the target directly.
///
/// With the RPC backend, RTT polling goes through the server-side `RttClient`
/// (created via the `create_rtt` endpoint and polled via `rtt/poll_up`),
/// collapsing the per-memory-read round-trip storm of a client-side
/// `RttClient` into one request per channel per poll.
#[derive(Clone)]
pub struct RttRemoteSeed {
    /// Tokio runtime handle used to bridge the synchronous DAP server to
    /// the async RPC client (same pattern as `RpcRemoteCore`).
    pub handle: Handle,
    /// RPC session interface used to issue RTT requests.
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

    /// If `Some`, the DAP server should drive the **server-side** RTT client
    /// over RPC (RPC backend). If `None`, it uses a local `RttClient`
    /// reading the target directly (local `Session` backend).
    fn rtt_remote_seed(&self) -> Option<RttRemoteSeed> {
        None
    }

    /// Build the stack frames for `core_index` while it is halted.
    ///
    /// This is `async` so the RPC backend can `.await` the
    /// `stack_trace/rich` round trip directly instead of bridging through
    /// `block_in_place` + `block_on` (see [`super::rpc::RpcBackend`]). The
    /// default implementation performs the unwind locally, reading target
    /// memory through the core returned by [`DapBackend::core`]; the RPC
    /// backend overrides this to issue a single round trip for the
    /// register-state unwind and then rebuild local variables from the
    /// supplied `debug_info` (which the DAP server holds locally),
    /// collapsing the per-memory-read round-trip storm into one request.
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
