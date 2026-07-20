//! RPC-backed [`DapBackend`] implementation.
//!
//! [`RpcBackend`] proxies all session/core operations to a probe-rs RPC
//! server through [`crate::rpc::client::RpcClient`]. Because the DAP server
//! is a synchronous debugger built on top of [`probe_rs::Core`], every
//! asynchronous RPC call is bridged back to a blocking call using
//! [`tokio::runtime::Handle::block_on`] inside a [`tokio::task::block_in_place`]
//! region. The DAP session loop itself must therefore be driven from a
//! [`tokio::task::spawn_blocking`] task on a multi-threaded runtime.
//!
//! `RpcRemoteCore` is the [`probe_rs::CoreInterface`] implementation that
//! wraps the async [`crate::rpc::client::CoreInterface`] and turns each call
//! into a synchronous one. A standard [`probe_rs::Core`] handle is built by
//! [`probe_rs::Core::from_boxed`] around it.

use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};

use probe_rs::{
    Architecture, BreakpointCause, Core, CoreInformation, CoreInterface, CoreRegister,
    CoreRegisters, CoreStatus, CoreType, Endian, Error, HaltReason, InstructionSet,
    MemoryInterface, RegisterId, RegisterRole, RegisterValue, Session, Target,
    VectorCatchCondition,
    semihosting::{Buffer, GetCommandLineRequest, SemihostingCommand},
};
use probe_rs_debug::{
    ColumnType, DebugInfo, DebugRegisters, ObjectRef, SourceLocation as DebugSourceLocation,
    StackFrame, SteppingMode, TypedPath,
};
use tokio::runtime::Handle;

use super::{DapBackend, FlashingBackend, block_on};
use crate::cmd::dap_server::DebuggerError;
use crate::cmd::dap_server::debug_adapter::dap::dap_types::{
    DisassembledInstruction, EvaluateArguments, EvaluateResponseBody, Scope, Source, Variable,
};
use crate::cmd::dap_server::server::configuration::FlashingConfig;
use crate::rpc::{
    Key,
    client::{CoreInterface as RpcCoreClient, RpcClient, SessionInterface},
    functions::{
        core_ops::{
            WireBreakpointCause, WireCoreStatus, WireHaltReason, WireRegisterId, WireRegisterValue,
            WireSemihostingCommand, WireVectorCatchCondition,
        },
        debug_vars::WireSteppingMode,
        disassemble::{WireDisassembledInstruction, WireSource},
        flash::{
            DownloadOptions as WireDownloadOptions, ProgressEvent as WireProgressEvent,
            VerifyResult,
        },
        stack_trace::{RichStackTraces, SourceLocation as WireSourceLocation, WireDebugRegister},
    },
};

/// Per-core cache of register values populated on demand from the bulk
/// `core/read_registers` endpoint. Shared across the short-lived
/// [`RpcRemoteCore`] instances produced by [`RpcBackend::core`] so that the
/// register dump that the DAP server performs on every halt becomes a
/// single round trip after the first register read.
type RegisterCache = HashMap<usize, HashMap<RegisterId, RegisterValue>>;

/// Per-core byte-oriented memory region cache. Stack unwinding and
/// variable evaluation issue many small, clustered reads (saved
/// registers on the stack, struct fields, ...). Each would otherwise be
/// its own RPC round trip through [`RpcRemoteCore`]. By prefetching a
/// modest aligned region on a miss and serving subsequent nearby reads
/// from the cache, a halt's worth of memory reads collapses from dozens
/// of round trips to roughly one per region touched.
type MemoryCache = HashMap<usize, Vec<MemoryRegion>>;

/// A contiguous cached byte range `[base, base + data.len())`.
#[derive(Clone)]
struct MemoryRegion {
    base: u64,
    data: Vec<u8>,
}

impl MemoryRegion {
    /// If this region fully covers `[addr, addr + len)`, return the offset
    /// of `addr` within it.
    fn covers(&self, addr: u64, len: usize) -> Option<usize> {
        let end = addr.checked_add(len as u64)?;
        if addr >= self.base && end <= self.base + self.data.len() as u64 {
            Some((addr - self.base) as usize)
        } else {
            None
        }
    }
}

/// Prefetch granularity for [`MemoryCache`]. Reads smaller than this are
/// served from a region of this size; larger reads bypass the cache.
const MEMORY_REGION_SIZE: usize = 2048;
/// Soft cap on the total cached bytes per core. Oldest regions are
/// evicted once the cap is exceeded.
const MEMORY_CACHE_MAX_BYTES: usize = 256 * 1024;

/// Interpret `bytes` (of length 1, 2, 4, or 8) as an unsigned integer
/// using the given endianness, returned as a `u64` for easy casting.
fn interpret_word(bytes: &[u8], endian: Endian) -> u64 {
    let be = matches!(endian, Endian::Big);
    match bytes.len() {
        1 => bytes[0] as u64,
        2 => {
            let arr: [u8; 2] = [bytes[0], bytes[1]];
            if be {
                u16::from_be_bytes(arr) as u64
            } else {
                u16::from_le_bytes(arr) as u64
            }
        }
        4 => {
            let arr: [u8; 4] = [bytes[0], bytes[1], bytes[2], bytes[3]];
            if be {
                u32::from_be_bytes(arr) as u64
            } else {
                u32::from_le_bytes(arr) as u64
            }
        }
        8 => {
            let arr: [u8; 8] = [
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ];
            if be {
                u64::from_be_bytes(arr)
            } else {
                u64::from_le_bytes(arr)
            }
        }
        _ => 0,
    }
}

/// Cache-aware read of `[addr, addr + len)` bytes. On a hit, serves from
/// `cache` with no fetch. On a miss, asks `fetch(base, count)` for an
/// aligned [`MEMORY_REGION_SIZE`] region, inserts it (evicting oldest
/// regions over [`MEMORY_CACHE_MAX_BYTES`]), and serves from it. Reads
/// larger than a region, and prefetches that error or short-read, fall
/// back to an exact fetch of just the requested range.
///
/// Split out from [`RpcRemoteCore::cached_read_bytes`] so the round-trip
/// behaviour can be unit-tested with a fake fetch.
fn cached_read_with<F>(
    cache: &mut Vec<MemoryRegion>,
    addr: u64,
    len: usize,
    mut fetch: F,
) -> Result<Vec<u8>, Error>
where
    F: FnMut(u64, usize) -> Result<Vec<u8>, Error>,
{
    // Large reads (disassemble, readMemory) gain nothing from caching and
    // would blow the cap; pass them straight through.
    if len > MEMORY_REGION_SIZE {
        return fetch(addr, len);
    }

    // Fast path: an existing region fully covers the request.
    for region in cache.iter() {
        if let Some(start) = region.covers(addr, len) {
            return Ok(region.data[start..start + len].to_vec());
        }
    }

    // Miss: prefetch an aligned region around the address.
    let region_base = (addr / MEMORY_REGION_SIZE as u64) * MEMORY_REGION_SIZE as u64;
    let region_data = match fetch(region_base, MEMORY_REGION_SIZE) {
        Ok(d) => d,
        Err(_) => {
            // The prefetched region may extend into unmapped memory that
            // the exact read would not touch; fall back.
            return fetch(addr, len);
        }
    };

    let region = MemoryRegion {
        base: region_base,
        data: region_data,
    };
    let start = (addr - region_base) as usize;
    if start + len > region.data.len() {
        // Short prefetched read doesn't cover the request; fall back to
        // an exact read so the caller gets the real result/error.
        return fetch(addr, len);
    }
    let out = region.data[start..start + len].to_vec();

    let mut total: usize = cache.iter().map(|r| r.data.len()).sum::<usize>() + region.data.len();
    while total > MEMORY_CACHE_MAX_BYTES && !cache.is_empty() {
        let removed = cache.first().map(|r| r.data.len()).unwrap_or(0);
        cache.remove(0);
        total -= removed;
    }
    cache.push(region);

    Ok(out)
}

/// Convert an [`anyhow::Error`] coming out of the RPC client into the
/// [`probe_rs::Error`] surface the DAP server expects.
fn rpc_err(err: anyhow::Error) -> Error {
    // Typed `probe_rs::Error` values lose their variant when crossing the
    // RPC boundary (they come back as an opaque `anyhow::Error` carrying
    // only the `Display` text). Best-effort reconstruct the common
    // variants that call sites care about, so features such as vector
    // catch silently fall back on targets that don't support them
    // instead of spamming ERROR logs.
    let text = format!("{err:#}");
    if text.contains("has not yet been implemented") {
        return Error::NotImplemented("remote operation");
    }
    Error::Other(format!("{err:?}"))
}

/// Rebuild a [`DebugRegisters`] from a wire register dump, using the same
/// static [`CoreRegisters`] file the server used so that the resulting
/// `DebugRegister` ordering and DWARF ids match a freshly-built
/// `DebugRegisters::from_core` (which is what the server's `unwind` starts
/// from). This keeps client-side `get_stackframe_info` calls consistent with
/// the server's.
fn rebuild_debug_registers(
    regs: &'static CoreRegisters,
    wire: &[WireDebugRegister],
) -> DebugRegisters {
    let mut lookup: HashMap<RegisterId, RegisterValue> = HashMap::with_capacity(wire.len());
    for w in wire {
        if let Some(value) = w.value {
            lookup.insert(w.id.into(), value.into());
        }
    }
    DebugRegisters::from_core_registers(regs, |rid| lookup.get(rid).copied())
}

/// Convert a wire [`SourceLocation`] (resolved server-side) back into a
/// `probe_rs_debug` [`SourceLocation`]. The wire form encodes `LeftEdge`
/// columns as `1`, so they round-trip as `Column(1)` — a minor, UI-irrelevant
/// difference for the RPC path.
fn from_wire_location(w: &WireSourceLocation) -> DebugSourceLocation {
    DebugSourceLocation {
        path: TypedPath::derive(w.file.as_bytes()).to_path_buf(),
        line: w.line,
        column: w.column.map(ColumnType::Column),
        address: None,
    }
}

/// A DAP backend that drives a remote target over RPC.
pub struct RpcBackend {
    handle: Handle,
    client: RpcClient,
    sessid: Key<Session>,
    cores: Vec<(usize, CoreType)>,
    /// A real `Target` obtained from the local registry by name. The object
    /// is never used for actual probe I/O on the client side; it only needs
    /// to supply `core_index_by_address`, memory-map metadata and similar
    /// introspection that the DAP server performs locally.
    target: Arc<Target>,
    /// Per-core metadata cached at attach-time so that [`CoreInterface`]
    /// methods that expect a synchronous answer (registers, is_64_bit, ...)
    /// can be served without a round trip.
    core_metadata: Vec<CoreMetadata>,
    /// Per-core register dump cache. See [`RegisterCache`].
    register_cache: RegisterCache,
    /// Per-core memory region cache. See [`MemoryCache`].
    memory_cache: MemoryCache,
}

#[derive(Clone)]
struct CoreMetadata {
    core_type: CoreType,
    architecture: Architecture,
    endian: Endian,
    is_64_bit: bool,
    fpu_support: bool,
    fp_register_count: Option<usize>,
    registers: &'static CoreRegisters,
}

impl RpcBackend {
    /// The RPC client backing this session, used for session-level
    /// operations that are not expressible through the [`DapBackend`] trait
    /// (eg. uploading a binary and issuing a flash over the wire).
    pub(crate) fn session_interface(&self) -> SessionInterface {
        SessionInterface::new(self.client.clone(), self.sessid)
    }

    /// Access the tokio runtime handle used to drive async RPC calls from a
    /// synchronous [`CoreInterface`] context.
    #[allow(
        dead_code,
        reason = "Kept as a symmetric accessor alongside `session_interface`; consumed by future iterations of the backend glue."
    )]
    pub(crate) fn tokio_handle(&self) -> Handle {
        self.handle.clone()
    }
}

#[allow(
    dead_code,
    reason = "new/session_interface helpers keep being invoked from later patches."
)]
impl RpcBackend {
    /// Build a new RPC backend.
    ///
    /// The caller is responsible for:
    /// * having already completed `probe/attach` over RPC (yielding a
    ///   `Key<Session>`),
    /// * producing a matching [`Target`] from the local chip registry (so
    ///   that memory-map and core-addressing introspection works without
    ///   extra round trips),
    /// * supplying per-core metadata: either by querying the server at
    ///   attach-time or by inferring it from the target description.
    pub fn new(
        handle: Handle,
        client: RpcClient,
        sessid: Key<Session>,
        target: Target,
        cores: Vec<(usize, CoreType)>,
        per_core: Vec<CorePerAttachInfo>,
    ) -> Self {
        let core_metadata = cores
            .iter()
            .zip(per_core)
            .map(|((_, core_type), info)| {
                let registers = CoreRegisters::for_core_type(
                    *core_type,
                    info.fpu_support,
                    info.fp_register_count,
                );
                CoreMetadata {
                    core_type: *core_type,
                    architecture: info.architecture,
                    endian: info.endian,
                    is_64_bit: info.is_64_bit,
                    fpu_support: info.fpu_support,
                    fp_register_count: info.fp_register_count,
                    registers,
                }
            })
            .collect();

        Self {
            handle,
            client,
            sessid,
            cores,
            target: Arc::new(target),
            core_metadata,
            register_cache: HashMap::new(),
            memory_cache: HashMap::new(),
        }
    }

    /// Drop the register dump and memory region caches for `core_index`.
    /// Called by the async `DapBackend` core-control overrides whenever an
    /// operation runs that could change registers or memory.
    fn invalidate_core_caches(&mut self, core_index: usize) {
        self.register_cache.remove(&core_index);
        if let Some(entry) = self.memory_cache.get_mut(&core_index) {
            entry.clear();
        }
    }
}

/// Per-core information the [`RpcBackend`] caller has to gather at attach
/// time. Most of these are static properties of the core type once the
/// target has booted, so a single query is enough.
#[derive(Clone, Copy)]
pub struct CorePerAttachInfo {
    pub architecture: Architecture,
    pub endian: Endian,
    pub is_64_bit: bool,
    pub fpu_support: bool,
    pub fp_register_count: Option<usize>,
}

impl DapBackend for RpcBackend {
    fn list_cores(&self) -> Vec<(usize, CoreType)> {
        self.cores.clone()
    }

    fn target(&self) -> &Target {
        &self.target
    }

    fn core(&mut self, core_index: usize) -> Result<Core<'_>, Error> {
        let metadata = self
            .cores
            .iter()
            .zip(self.core_metadata.iter())
            .find_map(|((idx, _), meta)| (*idx == core_index).then_some(meta.clone()))
            .ok_or(Error::CoreNotFound(core_index))?;

        let core = RpcRemoteCore {
            handle: self.handle.clone(),
            client: RpcCoreClient::new_for_backend(
                self.client.clone(),
                self.sessid,
                core_index as u32,
            ),
            metadata,
            core_index,
            register_cache: &mut self.register_cache,
            memory_cache: &mut self.memory_cache,
        };

        Ok(Core::from_boxed(
            core_index,
            &self.target.name,
            &self.target,
            Box::new(core),
        ))
    }

    fn rtt_remote_seed(&self) -> Option<super::RttRemoteSeed> {
        Some(super::RttRemoteSeed {
            handle: self.handle.clone(),
            session: self.session_interface(),
        })
    }

    /// `.await` the `core/status` round trip directly. The `GetCommandLine`
    /// semihosting variant still needs target memory reads, reconstructed
    /// via a short-lived [`RpcRemoteCore`].
    async fn status(&mut self, core_index: usize) -> Result<CoreStatus, Error> {
        let client =
            RpcCoreClient::new_for_backend(self.client.clone(), self.sessid, core_index as u32);
        let wire: WireCoreStatus = client.status().await.map_err(rpc_err)?;

        if let WireCoreStatus::Halted(WireHaltReason::Breakpoint(
            WireBreakpointCause::Semihosting(WireSemihostingCommand::GetCommandLine {
                block_address,
            }),
        )) = wire
        {
            let mut core = self.core(core_index)?;
            let buffer = Buffer::from_block_at(&mut core, block_address)?;
            return Ok(CoreStatus::Halted(HaltReason::Breakpoint(
                BreakpointCause::Semihosting(SemihostingCommand::GetCommandLine(
                    GetCommandLineRequest::new(buffer),
                )),
            )));
        }
        Ok(wire.into())
    }

    async fn set_hw_breakpoints(
        &mut self,
        core_index: usize,
        addresses: Vec<u64>,
    ) -> Result<Vec<bool>, Error> {
        let client =
            RpcCoreClient::new_for_backend(self.client.clone(), self.sessid, core_index as u32);
        client.set_hw_breakpoints(addresses).await.map_err(rpc_err)
    }

    async fn clear_hw_breakpoints(
        &mut self,
        core_index: usize,
        addresses: Vec<u64>,
    ) -> Result<(), Error> {
        let client =
            RpcCoreClient::new_for_backend(self.client.clone(), self.sessid, core_index as u32);
        client
            .clear_hw_breakpoints(addresses)
            .await
            .map_err(rpc_err)
    }

    async fn read_memory(
        &mut self,
        core_index: usize,
        address: u64,
        count: usize,
    ) -> Result<Vec<u8>, Error> {
        let client =
            RpcCoreClient::new_for_backend(self.client.clone(), self.sessid, core_index as u32);
        client.read_bytes(address, count).await.map_err(rpc_err)
    }

    async fn write_memory(
        &mut self,
        core_index: usize,
        address: u64,
        data: Vec<u8>,
    ) -> Result<(), Error> {
        let client =
            RpcCoreClient::new_for_backend(self.client.clone(), self.sessid, core_index as u32);
        client.write_memory_8(address, data).await.map_err(rpc_err)
    }

    async fn halt(
        &mut self,
        core_index: usize,
        timeout: Duration,
    ) -> Result<CoreInformation, Error> {
        self.invalidate_core_caches(core_index);
        let client =
            RpcCoreClient::new_for_backend(self.client.clone(), self.sessid, core_index as u32);
        let info = client.halt(timeout).await.map_err(rpc_err)?;
        Ok(info.into())
    }

    async fn run(&mut self, core_index: usize) -> Result<(), Error> {
        self.invalidate_core_caches(core_index);
        // The server-owned VariableCache is about to go stale: drop it so
        // stale variable handles are not served before the next halt
        // rebuilds them.
        self.session_interface()
            .clear_core_debug_state(core_index as u32)
            .await
            .map_err(rpc_err)?;
        let client =
            RpcCoreClient::new_for_backend(self.client.clone(), self.sessid, core_index as u32);
        client.run().await.map_err(rpc_err)
    }

    async fn reset_and_halt(
        &mut self,
        core_index: usize,
        timeout: Duration,
    ) -> Result<CoreInformation, Error> {
        self.invalidate_core_caches(core_index);
        let client =
            RpcCoreClient::new_for_backend(self.client.clone(), self.sessid, core_index as u32);
        let info = client.reset_and_halt(timeout).await.map_err(rpc_err)?;
        Ok(info.into())
    }

    async fn core_halted(&mut self, core_index: usize) -> Result<bool, Error> {
        let client =
            RpcCoreClient::new_for_backend(self.client.clone(), self.sessid, core_index as u32);
        client.core_halted().await.map_err(rpc_err)
    }

    async fn core_architecture(&mut self, core_index: usize) -> Result<Architecture, Error> {
        self.core_metadata
            .get(core_index)
            .map(|m| m.architecture)
            .ok_or_else(|| Error::Other(format!("No core metadata for core {core_index}")))
    }

    async fn program_counter_id(&mut self, core_index: usize) -> Result<RegisterId, Error> {
        self.core_metadata
            .get(core_index)
            .and_then(|m| m.registers.pc())
            .map(|r| r.id)
            .ok_or_else(|| Error::Other(format!("No PC register for core {core_index}")))
    }

    async fn set_hw_breakpoint(&mut self, core_index: usize, address: u64) -> Result<(), Error> {
        let client =
            RpcCoreClient::new_for_backend(self.client.clone(), self.sessid, core_index as u32);
        client.set_hw_breakpoint(address).await.map_err(rpc_err)
    }

    async fn clear_hw_breakpoint(&mut self, core_index: usize, address: u64) -> Result<(), Error> {
        let client =
            RpcCoreClient::new_for_backend(self.client.clone(), self.sessid, core_index as u32);
        client.clear_hw_breakpoint(address).await.map_err(rpc_err)
    }

    async fn set_variable(
        &mut self,
        core_index: usize,
        parent_key: i64,
        name: String,
        value: String,
    ) -> Result<crate::rpc::functions::debug_vars::WireSetVariableResponse, Error> {
        self.session_interface()
            .set_variable(core_index as u32, parent_key, name, value)
            .await
            .map_err(rpc_err)
    }

    async fn disassemble(
        &mut self,
        core_index: usize,
        _debug_info: Option<&probe_rs_debug::DebugInfo>,
        memory_reference: u64,
        byte_offset: i64,
        instruction_offset: i64,
        instruction_count: i64,
    ) -> Result<
        Vec<crate::cmd::dap_server::debug_adapter::dap::dap_types::DisassembledInstruction>,
        Error,
    > {
        let wire = self
            .session_interface()
            .disassemble(
                core_index as u32,
                memory_reference,
                byte_offset,
                instruction_offset,
                instruction_count,
            )
            .await
            .map_err(rpc_err)?;
        Ok(wire.into_iter().map(Into::into).collect())
    }

    async fn read_core_reg(
        &mut self,
        core_index: usize,
        register_id: RegisterId,
    ) -> Result<RegisterValue, Error> {
        let client =
            RpcCoreClient::new_for_backend(self.client.clone(), self.sessid, core_index as u32);
        let wire = client
            .read_core_reg(register_id.into())
            .await
            .map_err(rpc_err)?;
        Ok(wire.into())
    }

    async fn write_core_reg(
        &mut self,
        core_index: usize,
        register_id: RegisterId,
        value: RegisterValue,
    ) -> Result<(), Error> {
        self.invalidate_core_caches(core_index);
        let client =
            RpcCoreClient::new_for_backend(self.client.clone(), self.sessid, core_index as u32);
        client
            .write_core_reg(register_id.into(), value.into())
            .await
            .map_err(rpc_err)
    }

    async fn read_core_registers(
        &mut self,
        core_index: usize,
        ids: Vec<RegisterId>,
    ) -> Result<Vec<Option<RegisterValue>>, Error> {
        let wire_ids: Vec<WireRegisterId> = ids.iter().copied().map(Into::into).collect();
        let client =
            RpcCoreClient::new_for_backend(self.client.clone(), self.sessid, core_index as u32);
        let results = client
            .read_registers(wire_ids)
            .await
            .map_err(rpc_err)?;
        Ok(results
            .into_iter()
            .map(|r| r.value.map(RegisterValue::from))
            .collect())
    }

    fn register_file(
        &mut self,
        core_index: usize,
    ) -> Result<&'static probe_rs::CoreRegisters, Error> {
        Ok(self.core_metadata[core_index].registers)
    }

    async fn read_memory_8(
        &mut self,
        core_index: usize,
        address: u64,
        count: usize,
    ) -> Result<Vec<u8>, Error> {
        let client =
            RpcCoreClient::new_for_backend(self.client.clone(), self.sessid, core_index as u32);
        client
            .read_memory_8(address, count)
            .await
            .map_err(rpc_err)
    }

    async fn dump_core(
        &mut self,
        core_index: usize,
        ranges: Vec<std::ops::Range<u64>>,
    ) -> Result<probe_rs::CoreDump, Error> {
        let client =
            RpcCoreClient::new_for_backend(self.client.clone(), self.sessid, core_index as u32);
        let wire = client
            .dump_core(ranges)
            .await
            .map_err(rpc_err)?;
        Ok(probe_rs::CoreDump {
            registers: wire
                .registers
                .into_iter()
                .map(|(id, v)| (id.into(), v.into()))
                .collect(),
            data: wire.data,
            instruction_set: wire.instruction_set.into(),
            supports_native_64bit_access: wire.supports_native_64bit_access,
            core_type: wire.core_type.into(),
            fpu_support: wire.fpu_support,
            floating_point_register_count: wire.floating_point_register_count.map(|c| c as usize),
        })
    }

    async fn handle_semihosting(
        &mut self,
        core_index: usize,
        _state: &mut crate::cmd::dap_server::server::core_data::ClientSemihostingState,
    ) -> Result<crate::cmd::dap_server::backend::SemihostingHandleResult, Error> {
        use crate::cmd::dap_server::backend::{SemihostingHandleResult, SemihostingUiEvent};
        use crate::rpc::functions::core_ops::WireSemihostingUiEvent;

        let client =
            RpcCoreClient::new_for_backend(self.client.clone(), self.sessid, core_index as u32);
        let result = client.handle_semihosting().await.map_err(rpc_err)?;
        let events = result
            .events
            .into_iter()
            .map(|e| match e {
                WireSemihostingUiEvent::RttWindow { handle, path, format } => {
                    SemihostingUiEvent::RttWindow { handle, path, format }
                }
                WireSemihostingUiEvent::LogToConsole(s) => SemihostingUiEvent::LogToConsole(s),
                WireSemihostingUiEvent::RttOutput { handle, data } => {
                    SemihostingUiEvent::RttOutput { handle, data }
                }
            })
            .collect();
        Ok(SemihostingHandleResult {
            status: result.status.into(),
            events,
        })
    }

    async fn debug_step(
        &mut self,
        core_index: usize,
        mode: SteppingMode,
        _debug_info: Option<&DebugInfo>,
    ) -> Result<(CoreStatus, u64, Option<String>), Error> {
        self.invalidate_core_caches(core_index);
        let wire_mode = match mode {
            SteppingMode::StepInstruction => WireSteppingMode::StepInstruction,
            SteppingMode::OverStatement => WireSteppingMode::OverStatement,
            SteppingMode::IntoStatement => WireSteppingMode::IntoStatement,
            SteppingMode::OutOfStatement => WireSteppingMode::OutOfStatement,
            // Not exposed over the wire; map to a safe default.
            SteppingMode::BreakPoint => WireSteppingMode::OverStatement,
        };
        let resp = self
            .session_interface()
            .debug_step(core_index as u32, wire_mode)
            .await
            .map_err(rpc_err)?;
        Ok((resp.status.into(), resp.program_counter, resp.warning))
    }

    /// Single-round-trip stack unwind: the server unwinds AND owns the
    /// `local_variables`/`static_variables` caches (keyed by `sessid`+core),
    /// returning per-frame metadata plus the server-assigned frame id. We
    /// rebuild lightweight [`StackFrame`]s with `local_variables: None` —
    /// subsequent `scopes`/`variables` requests resolve server-side — so the
    /// per-memory-read round trips of the old client-side `get_stackframe_info`
    /// rebuild are gone. Source locations are taken from the wire (the server
    /// already resolved them).
    async fn unwind_stack(
        &mut self,
        core_index: usize,
        program_binary: Option<&Path>,
        _debug_info: &DebugInfo,
        max_frames: usize,
    ) -> Result<Vec<StackFrame>, Error> {
        let path = program_binary
            .ok_or_else(|| Error::Other("program_binary required for RPC stack trace".into()))?
            .to_path_buf();

        let session = self.session_interface();
        let rich: RichStackTraces = session
            .take_rich_stack_trace(path, max_frames as u32)
            .await
            .map_err(rpc_err)?;

        let rich_core = rich
            .cores
            .into_iter()
            .find(|c| c.core == core_index as u32)
            .ok_or_else(|| {
                Error::Other(format!("core {core_index} missing from rich stack trace"))
            })?;

        // Clone metadata before borrowing `self` via `self.core(...)`.
        let metadata = self
            .cores
            .iter()
            .zip(self.core_metadata.iter())
            .find_map(|((idx, _), meta)| (*idx == core_index).then_some(meta.clone()))
            .ok_or(Error::CoreNotFound(core_index))?;

        let wire_frames = rich_core.frames;
        let mut frames: Vec<StackFrame> = Vec::with_capacity(wire_frames.len());

        let mut idx = 0;
        while idx < wire_frames.len() {
            let group_start = idx;
            let group_regs = wire_frames[idx].registers.clone();
            while idx < wire_frames.len() && wire_frames[idx].registers == group_regs {
                idx += 1;
            }
            let group = &wire_frames[group_start..idx];
            let cfa = group[0].canonical_frame_address;
            let registers = rebuild_debug_registers(metadata.registers, &group_regs);

            for wf in group {
                let pc_value: RegisterValue = wf.program_counter.into();
                frames.push(StackFrame {
                    id: ObjectRef::from(wf.id as i64),
                    function_name: wf.function_name.clone(),
                    source_location: wf.location.as_ref().map(from_wire_location),
                    registers: registers.clone(),
                    pc: pc_value,
                    frame_base: wf.frame_base,
                    is_inlined: wf.is_inlined,
                    local_variables: None,
                    canonical_frame_address: cfa,
                });
            }
        }

        Ok(frames)
    }

    async fn scopes(
        &mut self,
        core_index: usize,
        frame_id: u32,
    ) -> Result<Option<Vec<Scope>>, Error> {
        let session = self.session_interface();
        let wire = session
            .scopes(core_index as u32, frame_id)
            .await
            .map_err(rpc_err)?;
        Ok(Some(
            wire.into_iter()
                .map(|w| Scope {
                    name: w.name,
                    presentation_hint: w.presentation_hint,
                    variables_reference: w.variables_reference,
                    named_variables: w.named_variables,
                    indexed_variables: w.indexed_variables,
                    expensive: w.expensive,
                    line: w.line,
                    column: w.column,
                    end_line: w.end_line,
                    end_column: w.end_column,
                    source: None,
                })
                .collect(),
        ))
    }

    async fn variables(
        &mut self,
        core_index: usize,
        variables_reference: u32,
        filter: Option<String>,
    ) -> Result<Option<Vec<Variable>>, Error> {
        let session = self.session_interface();
        let wire = session
            .variables(core_index as u32, variables_reference, filter)
            .await
            .map_err(rpc_err)?;
        Ok(Some(
            wire.into_iter()
                .map(|w| Variable {
                    name: w.name,
                    evaluate_name: w.evaluate_name,
                    memory_reference: w.memory_reference,
                    indexed_variables: w.indexed_variables,
                    named_variables: w.named_variables,
                    presentation_hint: None,
                    type_: w.type_,
                    value: w.value,
                    variables_reference: w.variables_reference,
                    declaration_location_reference: None,
                    value_location_reference: None,
                })
                .collect(),
        ))
    }

    async fn evaluate(
        &mut self,
        core_index: usize,
        arguments: &EvaluateArguments,
    ) -> Result<Option<EvaluateResponseBody>, Error> {
        // Only watch/hover go server-side; repl/clipboard stay local (the
        // REPL drives the core directly via the bridge, and clipboard is
        // just the expression text).
        if !matches!(arguments.context.as_deref(), Some("watch") | Some("hover")) {
            return Ok(None);
        }
        let session = self.session_interface();
        let wire = session
            .evaluate(
                core_index as u32,
                arguments.frame_id.map(|id| id as u32),
                arguments.context.clone().unwrap_or_default(),
                arguments.expression.clone(),
            )
            .await
            .map_err(rpc_err)?;
        Ok(Some(EvaluateResponseBody {
            result: wire.result,
            type_: wire.type_,
            variables_reference: wire.variables_reference,
            named_variables: wire.named_variables,
            indexed_variables: wire.indexed_variables,
            memory_reference: wire.memory_reference,
            presentation_hint: None,
            value_location_reference: None,
        }))
    }
}

/// Synchronous [`CoreInterface`] implementation backed by an async RPC client.
pub struct RpcRemoteCore<'a> {
    handle: Handle,
    client: RpcCoreClient,
    metadata: CoreMetadata,
    core_index: usize,
    register_cache: &'a mut RegisterCache,
    memory_cache: &'a mut MemoryCache,
}

impl RpcRemoteCore<'_> {
    /// Invalidate this core's cached register dump. Called whenever an
    /// operation is issued that could plausibly change register contents:
    /// `run`, `step`, `halt`, any reset, or a register write.
    fn invalidate_register_cache(&mut self) {
        self.register_cache.remove(&self.core_index);
    }

    /// Drop both the register dump and the memory region caches for this
    /// core. Used by operations that resume or reset the core, since the
    /// program may have changed both registers and memory in the meantime.
    fn invalidate_caches(&mut self) {
        self.invalidate_register_cache();
        self.invalidate_memory_cache();
    }

    /// Drop all cached memory regions for this core. Called whenever an
    /// operation is issued that could change memory contents: `run`,
    /// `step`, `halt`, any reset, a register write, or a memory write.
    fn invalidate_memory_cache(&mut self) {
        if let Some(entry) = self.memory_cache.get_mut(&self.core_index) {
            entry.clear();
        }
    }

    /// Read `[addr, addr + len)` bytes, serving from the per-core region
    /// cache when possible and prefetching an aligned region on a miss.
    /// Reads larger than [`MEMORY_REGION_SIZE`] bypass the cache. The
    /// returned `Vec` is a copy so callers can release the borrow on
    /// `self` before interpreting the bytes into typed words.
    fn cached_read_bytes(&mut self, addr: u64, len: usize) -> Result<Vec<u8>, Error> {
        let handle = &self.handle;
        let client = &self.client;
        let cache = self.memory_cache.entry(self.core_index).or_default();
        cached_read_with(cache, addr, len, |a, n| {
            block_on(handle, client.read_memory_8(a, n)).map_err(rpc_err)
        })
    }

    /// Interpret `bytes` (of length 1, 2, 4, or 8) as an unsigned integer
    /// using this core's endianness, returned as a `u64` for easy casting.
    fn read_word_le_be(&self, bytes: &[u8]) -> u64 {
        interpret_word(bytes, self.metadata.endian)
    }

    /// Look up a single register, refilling the cache with a batched
    /// `core/read_registers` call on a miss.
    fn cached_read_reg(&mut self, id: RegisterId) -> Result<RegisterValue, Error> {
        if let Some(entry) = self.register_cache.get(&self.core_index)
            && let Some(value) = entry.get(&id)
        {
            return Ok(*value);
        }

        self.refill_register_cache()?;

        if let Some(entry) = self.register_cache.get(&self.core_index)
            && let Some(value) = entry.get(&id)
        {
            return Ok(*value);
        }

        // The batched read did not return the requested register (either
        // the target refused to read it, or it is not part of the static
        // register file). Fall back to a direct single read so the caller
        // still gets an authoritative answer / error.
        let wire: WireRegisterValue =
            block_on(&self.handle, self.client.read_core_reg(id.into())).map_err(rpc_err)?;
        Ok(wire.into())
    }

    /// Issue a single `core/read_registers` call covering every register in
    /// this core's static register file (including FP registers when
    /// available) and populate the cache with whatever the server returns.
    fn refill_register_cache(&mut self) -> Result<(), Error> {
        let mut ids: Vec<RegisterId> = self
            .metadata
            .registers
            .core_registers()
            .map(|r| r.id())
            .collect();
        if self.metadata.fpu_support
            && let Some(fpu) = self.metadata.registers.fpu_registers()
        {
            ids.extend(fpu.map(|r| r.id()));
        }
        let wire_ids = ids.iter().copied().map(Into::into).collect();

        let results =
            block_on(&self.handle, self.client.read_registers(wire_ids)).map_err(rpc_err)?;

        let entry = self.register_cache.entry(self.core_index).or_default();
        for result in results {
            if let Some(value) = result.value {
                entry.insert(result.id.into(), value.into());
            }
        }
        Ok(())
    }
}

/// Helper that resolves a single [`CoreRegister`] from the static register
/// table by role, or panics with a descriptive message if the target is
/// misconfigured.
///
/// The panic on a missing register mirrors [`probe_rs::Core`]'s own
/// assumption that every supported target has a stack pointer / frame
/// pointer / return address / program counter in its register file.
#[allow(
    clippy::panic,
    reason = "mirrors probe_rs::Core's invariants about the register file"
)]
fn register_with_role(
    registers: &'static CoreRegisters,
    role: RegisterRole,
    name: &'static str,
) -> &'static CoreRegister {
    registers
        .core_registers()
        .find(|r| r.register_has_role(role))
        .unwrap_or_else(|| panic!("register set is missing the {name} register"))
}

// Generate the 4 `read_word_N` / `read_N` / `write_word_N` / `write_N`
// methods for a given word width `$n`. The shape is identical across
// sizes and only the RPC method names differ, so we stamp them out with
// a macro instead of hand-writing 16 nearly-identical functions.
macro_rules! rpc_mem_methods {
    ($n:literal, $ty:ty, $read_word:ident, $read_many:ident, $write_word:ident,
     $write_many:ident, $rpc_read:ident, $rpc_write:ident) => {
        fn $read_word(&mut self, address: u64) -> Result<$ty, Error> {
            let bytes = self.cached_read_bytes(address, std::mem::size_of::<$ty>())?;
            Ok(self.read_word_le_be(&bytes) as $ty)
        }

        fn $read_many(&mut self, address: u64, data: &mut [$ty]) -> Result<(), Error> {
            let ws = std::mem::size_of::<$ty>();
            let nbytes = data.len() * ws;
            let bytes = self.cached_read_bytes(address, nbytes)?;
            for (i, slot) in data.iter_mut().enumerate() {
                let off = i * ws;
                *slot = self.read_word_le_be(&bytes[off..off + ws]) as $ty;
            }
            Ok(())
        }

        fn $write_word(&mut self, address: u64, data: $ty) -> Result<(), Error> {
            self.invalidate_memory_cache();
            block_on(&self.handle, self.client.$rpc_write(address, vec![data])).map_err(rpc_err)
        }

        fn $write_many(&mut self, address: u64, data: &[$ty]) -> Result<(), Error> {
            self.invalidate_memory_cache();
            block_on(&self.handle, self.client.$rpc_write(address, data.to_vec())).map_err(rpc_err)
        }
    };
}

impl MemoryInterface for RpcRemoteCore<'_> {
    fn supports_native_64bit_access(&mut self) -> bool {
        self.metadata.is_64_bit
    }

    rpc_mem_methods!(
        8,
        u8,
        read_word_8,
        read_8,
        write_word_8,
        write_8,
        read_memory_8,
        write_memory_8
    );
    rpc_mem_methods!(
        16,
        u16,
        read_word_16,
        read_16,
        write_word_16,
        write_16,
        read_memory_16,
        write_memory_16
    );
    rpc_mem_methods!(
        32,
        u32,
        read_word_32,
        read_32,
        write_word_32,
        write_32,
        read_memory_32,
        write_memory_32
    );
    rpc_mem_methods!(
        64,
        u64,
        read_word_64,
        read_64,
        write_word_64,
        write_64,
        read_memory_64,
        write_memory_64
    );

    fn supports_8bit_transfers(&self) -> Result<bool, Error> {
        Ok(true)
    }

    fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

impl CoreInterface for RpcRemoteCore<'_> {
    fn wait_for_core_halted(&mut self, timeout: Duration) -> Result<(), Error> {
        block_on(&self.handle, self.client.wait_for_core_halted(timeout)).map_err(rpc_err)
    }

    fn core_halted(&mut self) -> Result<bool, Error> {
        block_on(&self.handle, self.client.core_halted()).map_err(rpc_err)
    }

    fn status(&mut self) -> Result<CoreStatus, Error> {
        let wire: WireCoreStatus = block_on(&self.handle, self.client.status()).map_err(rpc_err)?;
        // For most variants, the canonical `From` conversion is lossless
        // enough. The exception is `Semihosting(GetCommandLine { .. })`:
        // the wire only carries the target block address, so we rebuild
        // a real `GetCommandLineRequest` bound to this core — its
        // subsequent `write_command_line_to_target` call will then flow
        // through our `MemoryInterface` (i.e. over memory RPCs) and
        // complete the handshake end-to-end.
        if let WireCoreStatus::Halted(WireHaltReason::Breakpoint(
            WireBreakpointCause::Semihosting(WireSemihostingCommand::GetCommandLine {
                block_address,
            }),
        )) = wire
        {
            let buffer = Buffer::from_block_at(self, block_address)?;
            return Ok(CoreStatus::Halted(HaltReason::Breakpoint(
                BreakpointCause::Semihosting(SemihostingCommand::GetCommandLine(
                    GetCommandLineRequest::new(buffer),
                )),
            )));
        }
        Ok(wire.into())
    }

    fn halt(&mut self, timeout: Duration) -> Result<CoreInformation, Error> {
        self.invalidate_caches();
        let info = block_on(&self.handle, self.client.halt(timeout)).map_err(rpc_err)?;
        Ok(info.into())
    }

    fn run(&mut self) -> Result<(), Error> {
        self.invalidate_caches();
        block_on(&self.handle, self.client.run()).map_err(rpc_err)
    }

    fn reset(&mut self) -> Result<(), Error> {
        self.invalidate_caches();
        block_on(&self.handle, self.client.reset()).map_err(rpc_err)
    }

    fn reset_and_halt(&mut self, timeout: Duration) -> Result<CoreInformation, Error> {
        self.invalidate_caches();
        block_on(&self.handle, self.client.reset_and_halt(timeout)).map_err(rpc_err)?;
        // The existing `reset_and_halt` endpoint only returns `()`; the PC
        // will be read by the next call anyway. Surface a zero-filled
        // `CoreInformation` until a richer endpoint is wired up.
        Ok(CoreInformation { pc: 0 })
    }

    fn step(&mut self) -> Result<CoreInformation, Error> {
        self.invalidate_caches();
        let info = block_on(&self.handle, self.client.step()).map_err(rpc_err)?;
        Ok(info.into())
    }

    fn read_core_reg(&mut self, address: RegisterId) -> Result<RegisterValue, Error> {
        self.cached_read_reg(address)
    }

    fn write_core_reg(&mut self, address: RegisterId, value: RegisterValue) -> Result<(), Error> {
        self.invalidate_register_cache();
        block_on(
            &self.handle,
            self.client.write_core_reg(address.into(), value.into()),
        )
        .map_err(rpc_err)
    }

    fn available_breakpoint_units(&mut self) -> Result<u32, Error> {
        block_on(&self.handle, self.client.available_breakpoint_units()).map_err(rpc_err)
    }

    fn hw_breakpoints(&mut self) -> Result<Vec<Option<u64>>, Error> {
        // The current RPC surface exposes breakpoint management as
        // address-based set/clear operations (the server performs unit
        // allocation). We therefore do not expose the raw breakpoint unit
        // table. `Core::set_hw_breakpoint(addr)` must not be used on a
        // remote core; callers should invoke `set_hw_breakpoint` directly
        // on this `CoreInterface` and pass `0` for the unit index.
        Err(Error::NotImplemented(
            "hw_breakpoints over RPC; use set_hw_breakpoint(addr) directly",
        ))
    }

    fn enable_breakpoints(&mut self, _state: bool) -> Result<(), Error> {
        // Breakpoints are enabled implicitly by the server when
        // `core/set_hw_bp` is invoked.
        Ok(())
    }

    fn set_hw_breakpoint(&mut self, _unit_index: usize, addr: u64) -> Result<(), Error> {
        // `unit_index` is ignored: server-side `core/set_hw_bp` performs its
        // own allocation.
        block_on(&self.handle, self.client.set_hw_breakpoint(addr)).map_err(rpc_err)
    }

    fn clear_hw_breakpoint(&mut self, _unit_index: usize) -> Result<(), Error> {
        // With the address-based endpoint we cannot clear by unit index
        // alone. DAP code paths that reach this trait method go through
        // `Core::clear_hw_breakpoint(addr)`, which resolves the address
        // first via `hw_breakpoints()` - but we returned `NotImplemented`
        // from that, so callers must use the address-based path instead.
        Err(Error::NotImplemented(
            "clear_hw_breakpoint by unit index; use the address-based path",
        ))
    }

    fn registers(&self) -> &'static CoreRegisters {
        self.metadata.registers
    }

    fn program_counter(&self) -> &'static CoreRegister {
        #[allow(
            clippy::expect_used,
            reason = "mirrors probe_rs::Core's invariant that every supported core has a PC"
        )]
        self.metadata
            .registers
            .pc()
            .expect("register set must contain a program counter")
    }

    fn frame_pointer(&self) -> &'static CoreRegister {
        register_with_role(
            self.metadata.registers,
            RegisterRole::FramePointer,
            "frame pointer",
        )
    }

    fn stack_pointer(&self) -> &'static CoreRegister {
        register_with_role(
            self.metadata.registers,
            RegisterRole::StackPointer,
            "stack pointer",
        )
    }

    fn return_address(&self) -> &'static CoreRegister {
        register_with_role(
            self.metadata.registers,
            RegisterRole::ReturnAddress,
            "return address",
        )
    }

    fn hw_breakpoints_enabled(&self) -> bool {
        true
    }

    fn architecture(&self) -> Architecture {
        self.metadata.architecture
    }

    fn core_type(&self) -> CoreType {
        self.metadata.core_type
    }

    fn instruction_set(&mut self) -> Result<InstructionSet, Error> {
        let wire = block_on(&self.handle, self.client.instruction_set()).map_err(rpc_err)?;
        Ok(wire.into())
    }

    fn endianness(&mut self) -> Result<Endian, Error> {
        Ok(self.metadata.endian)
    }

    fn fpu_support(&mut self) -> Result<bool, Error> {
        Ok(self.metadata.fpu_support)
    }

    fn floating_point_register_count(&mut self) -> Result<usize, Error> {
        Ok(self.metadata.fp_register_count.unwrap_or(0))
    }

    fn reset_catch_set(&mut self) -> Result<(), Error> {
        // Reset-catch is not currently exposed over RPC.
        Err(Error::NotImplemented("reset_catch_set over RPC"))
    }

    fn reset_catch_clear(&mut self) -> Result<(), Error> {
        Err(Error::NotImplemented("reset_catch_clear over RPC"))
    }

    fn debug_core_stop(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn enable_vector_catch(&mut self, condition: VectorCatchCondition) -> Result<(), Error> {
        let wire: WireVectorCatchCondition = condition.into();
        block_on(&self.handle, self.client.enable_vector_catch(wire)).map_err(rpc_err)
    }

    fn disable_vector_catch(&mut self, condition: VectorCatchCondition) -> Result<(), Error> {
        let wire: WireVectorCatchCondition = condition.into();
        block_on(&self.handle, self.client.disable_vector_catch(wire)).map_err(rpc_err)
    }

    fn is_64_bit(&self) -> bool {
        self.metadata.is_64_bit
    }
}

impl FlashingBackend for RpcBackend {
    async fn flash_binary(
        &mut self,
        path_to_elf: &Path,
        config: &FlashingConfig,
        progress: &mut dyn FnMut(WireProgressEvent),
    ) -> Result<(), DebuggerError> {
        let session = self.session_interface();

        let build_result = session
            .build_flash_loader(
                path_to_elf.to_path_buf(),
                config.format_options.clone(),
                None,
                false,
            )
            .await
            .map_err(|e| DebuggerError::Other(anyhow::anyhow!(e)))?;

        let loader_key = build_result.loader;

        let run_flash = if config.verify_before_flashing {
            match session
                .verify(loader_key, async |event| {
                    progress(event);
                })
                .await
                .map_err(|e| DebuggerError::Other(anyhow::anyhow!(e)))?
            {
                VerifyResult::Ok => false,
                VerifyResult::Mismatch => true,
            }
        } else {
            true
        };

        if run_flash {
            let options = WireDownloadOptions {
                keep_unwritten_bytes: config.restore_unwritten_bytes,
                do_chip_erase: config.full_chip_erase,
                skip_erase: false,
                verify: config.verify_after_flashing,
                disable_double_buffering: false,
                preferred_algos: Vec::new(),
            };

            session
                .flash(options, loader_key, None, async |event| {
                    progress(event);
                })
                .await
                .map_err(|e| DebuggerError::Other(anyhow::anyhow!(e)))?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake fetch that returns deterministic bytes and counts how many
    /// RPC round trips it would have issued. Each fetch returns `count`
    /// bytes whose values are derived from `base`, so tests can verify the
    /// right range was fetched and that cached reads return the right data.
    struct CountingFetch {
        calls: usize,
        log: Vec<(u64, usize)>,
    }

    impl CountingFetch {
        fn new() -> Self {
            Self {
                calls: 0,
                log: Vec::new(),
            }
        }

        fn fetch(&mut self, base: u64, count: usize) -> Result<Vec<u8>, Error> {
            self.calls += 1;
            self.log.push((base, count));
            Ok((0..count)
                .map(|i| base.wrapping_add(i as u64) as u8)
                .collect())
        }
    }

    #[test]
    fn clustered_reads_collapse_into_one_fetch() {
        let mut cache = Vec::new();
        let mut fetcher = CountingFetch::new();

        // A handful of small reads within the same 2 KB region, mirroring
        // the access pattern of stack unwinding (saved registers at SP +
        // small offsets).
        for off in [0u64, 4, 8, 12, 16, 20, 24, 28] {
            let addr = 0x1000_0000 + off;
            let bytes = cached_read_with(&mut cache, addr, 4, |a, n| fetcher.fetch(a, n)).unwrap();
            // The fake fetch returns byte `i` == `(base + i) as u8`.
            let expect: Vec<u8> = (0..4u64).map(|i| (addr + i) as u8).collect();
            assert_eq!(bytes, expect);
        }

        // One prefetch covers all eight reads.
        assert_eq!(fetcher.calls, 1);
        assert_eq!(fetcher.log, vec![(0x1000_0000, MEMORY_REGION_SIZE)]);
    }

    #[test]
    fn reads_in_different_regions_fetch_once_each() {
        let mut cache = Vec::new();
        let mut fetcher = CountingFetch::new();

        cached_read_with(&mut cache, 0x0000, 4, |a, n| fetcher.fetch(a, n)).unwrap();
        cached_read_with(&mut cache, 0x0800, 4, |a, n| fetcher.fetch(a, n)).unwrap();
        cached_read_with(&mut cache, 0x1000, 4, |a, n| fetcher.fetch(a, n)).unwrap();
        // Re-reading the first region is a cache hit (no new fetch).
        cached_read_with(&mut cache, 0x0010, 4, |a, n| fetcher.fetch(a, n)).unwrap();

        assert_eq!(fetcher.calls, 3);
    }

    #[test]
    fn large_read_bypasses_cache() {
        let mut cache = Vec::new();
        let mut fetcher = CountingFetch::new();

        let len = MEMORY_REGION_SIZE + 64;
        cached_read_with(&mut cache, 0x2000, len, |a, n| fetcher.fetch(a, n)).unwrap();

        assert_eq!(fetcher.calls, 1);
        assert_eq!(fetcher.log, vec![(0x2000, len)]);
        assert!(cache.is_empty());
    }

    #[test]
    fn prefetch_error_falls_back_to_exact_read() {
        let mut cache = Vec::new();
        let mut fetcher = CountingFetch::new();

        let bytes = cached_read_with(&mut cache, 0x0, 4, |a, n| {
            if n == MEMORY_REGION_SIZE {
                Err(Error::Other("unmapped".into()))
            } else {
                fetcher.fetch(a, n)
            }
        })
        .unwrap();

        assert_eq!(bytes, vec![0x00, 0x01, 0x02, 0x03]);
        assert_eq!(fetcher.calls, 1);
        assert!(cache.is_empty());
    }

    #[test]
    fn region_covers_helper() {
        let region = MemoryRegion {
            base: 0x1000,
            data: vec![0; 64],
        };
        assert_eq!(region.covers(0x1000, 4), Some(0));
        assert_eq!(region.covers(0x1000 + 60, 4), Some(60));
        assert_eq!(region.covers(0x1000 + 60, 8), None);
        assert_eq!(region.covers(0x0FFC, 8), None);
    }

    #[test]
    fn interpret_word_endianness() {
        let bytes = [0x01, 0x02, 0x03, 0x04];
        assert_eq!(interpret_word(&bytes, Endian::Little), 0x0403_0201);
        assert_eq!(interpret_word(&bytes, Endian::Big), 0x0102_0304);
        assert_eq!(interpret_word(&[0xAB], Endian::Little), 0xAB);
        let b8 = [1u8, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(interpret_word(&b8, Endian::Little), 0x0807_0605_0403_0201);
        assert_eq!(interpret_word(&b8, Endian::Big), 0x0102_0304_0506_0708);
    }
}

impl From<WireSource> for Source {
    fn from(s: WireSource) -> Self {
        Source {
            name: s.name,
            path: s.path,
            source_reference: None,
            presentation_hint: None,
            origin: None,
            sources: None,
            adapter_data: None,
            checksums: None,
        }
    }
}

impl From<WireDisassembledInstruction> for DisassembledInstruction {
    fn from(i: WireDisassembledInstruction) -> Self {
        DisassembledInstruction {
            address: i.address,
            column: i.column,
            end_column: None,
            end_line: None,
            instruction: i.instruction,
            instruction_bytes: i.instruction_bytes,
            line: i.line,
            location: i.location.map(Source::from),
            symbol: None,
            presentation_hint: None,
        }
    }
}
