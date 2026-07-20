use std::fmt::{self, Display, Write as _};

use crate::rpc::{
    Key,
    functions::{
        NoResponse, RpcContext, RpcResult,
        core_ops::{WireRegisterId, WireRegisterValue},
    },
};
use postcard_rpc::header::VarHeader;
use postcard_schema::Schema;
use probe_rs::{Error, Session};
use probe_rs_debug::{
    DebugInfo, DebugRegister, DebugRegisters, StackFrame, VariableCache, exception_handler_for_core,
};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Schema)]
pub struct StackTraces {
    pub cores: Vec<StackTrace>,
}

#[derive(Serialize, Deserialize, Schema)]
pub struct StackTrace {
    pub core: u32,
    pub frames: Vec<StackTraceFrame>,
}

#[derive(Serialize, Deserialize, Schema)]
pub struct StackTraceFrame {
    pub function_name: String,
    pub program_counter: u64,
    pub is_inlined: bool,
    pub location: Option<SourceLocation>,
}

impl From<&probe_rs_debug::SourceLocation> for SourceLocation {
    fn from(location: &probe_rs_debug::SourceLocation) -> Self {
        SourceLocation {
            file: location.path.to_path().display().to_string(),
            line: location.line,
            column: location.column.map(|col| match col {
                probe_rs_debug::ColumnType::LeftEdge => 1,
                probe_rs_debug::ColumnType::Column(c) => c,
            }),
        }
    }
}

impl From<StackFrame> for StackTraceFrame {
    fn from(frame: StackFrame) -> Self {
        StackTraceFrame::from(&frame)
    }
}

impl From<&StackFrame> for StackTraceFrame {
    fn from(frame: &StackFrame) -> Self {
        StackTraceFrame {
            function_name: frame.function_name.clone(),
            program_counter: frame.pc.try_into().unwrap_or(0),
            is_inlined: frame.is_inlined,
            location: frame
                .source_location
                .as_ref()
                .map(|location| SourceLocation {
                    file: location.path.to_path().display().to_string(),
                    line: location.line,
                    column: location.column.map(|col| match col {
                        probe_rs_debug::ColumnType::LeftEdge => 1,
                        probe_rs_debug::ColumnType::Column(c) => c,
                    }),
                }),
        }
    }
}

impl Display for StackTraceFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut output_stream = String::new();
        write!(f, "{} @ {:x}", self.function_name, self.program_counter).unwrap();

        if self.is_inlined {
            write!(&mut output_stream, " inline").unwrap();
        }
        f.write_str("\n")?;

        if let Some(location) = &self.location {
            write!(f, "       {location}")?;
        }

        Ok(())
    }
}

#[derive(Serialize, Deserialize, Schema, Clone)]
pub struct SourceLocation {
    pub file: String,
    pub line: Option<u64>,
    pub column: Option<u64>,
}

impl Display for SourceLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.file)?;
        if let Some(line) = self.line {
            write!(f, ":{line}")?;
            if let Some(column) = self.column {
                write!(f, ":{column}")?;
            }
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Schema)]
pub struct TakeStackTraceRequest {
    pub sessid: Key<Session>,
    pub path: String,
    pub stack_frame_limit: u32,
}

pub type TakeStackTraceResponse = RpcResult<StackTraces>;

#[derive(Serialize, Deserialize, Schema)]
pub struct LoadDebugInfoRequest {
    pub sessid: Key<Session>,
    pub path: String,
}

pub type LoadDebugInfoResponse = NoResponse;

/// Eagerly load and cache the server-side [`DebugInfo`] for a session, keyed
/// by `sessid`. This mirrors the local backend, which loads `DebugInfo` from
/// the program binary at session start, so server-side consumers (notably
/// `disassemble`) can resolve source locations before the first halt (which
/// is when `take_rich_stack_trace` would otherwise populate it). Idempotent:
/// if a [`ServerDebugState`] already exists for the session it is left
/// untouched.
pub async fn load_debug_info(
    ctx: &mut RpcContext,
    _header: VarHeader,
    request: LoadDebugInfoRequest,
) -> LoadDebugInfoResponse {
    let states = ctx.debug_states();
    let mut guard = states.lock().await;
    if guard.contains_key(&request.sessid) {
        return Ok(());
    }
    let debug_info = DebugInfo::from_file(&request.path).map_err(|e| e.to_string())?;
    let state = crate::rpc::debug_state::ServerDebugState::new(debug_info);
    guard.insert(request.sessid, state);
    Ok(())
}

/// Shared per-core unwind loop used by both [`take_stack_trace`] and
/// [`take_rich_stack_trace`]. Returns `(core_index, frames)` pairs, where
/// each `StackFrame` is converted to `F` via `F::from`.
async fn unwind_all_cores<F: From<StackFrame>>(
    ctx: &mut RpcContext,
    request: &TakeStackTraceRequest,
) -> RpcResult<Vec<(u32, Vec<F>)>> {
    let mut session = ctx.session(request.sessid).await;

    let Some(debug_info) = DebugInfo::from_file(&request.path).ok() else {
        Err("No debug info found.")?
    };

    session
        .halted_access(|session| {
            let mut cores = Vec::new();
            for (idx, core_type) in session.list_cores() {
                let mut core = match session.core(idx) {
                    Ok(core) => core,
                    Err(Error::CoreDisabled(_)) => continue,
                    Err(e) => return Err(e),
                };

                let initial_registers = DebugRegisters::from_core(&mut core);
                let exception_interface = exception_handler_for_core(core_type);
                let instruction_set = core.instruction_set().ok();
                let stack_frames = debug_info.unwind(
                    &mut core,
                    initial_registers,
                    exception_interface.as_ref(),
                    instruction_set,
                    request.stack_frame_limit as usize,
                )?;

                let frames: Vec<F> = stack_frames.into_iter().map(F::from).collect();
                cores.push((idx as u32, frames));
            }
            Ok(cores)
        })
        .map_err(Into::into)
}

pub async fn take_stack_trace(
    ctx: &mut RpcContext,
    _header: VarHeader,
    request: TakeStackTraceRequest,
) -> TakeStackTraceResponse {
    let cores = unwind_all_cores::<StackTraceFrame>(ctx, &request).await?;
    Ok(StackTraces {
        cores: cores
            .into_iter()
            .map(|(core, frames)| StackTrace { core, frames })
            .collect(),
    })
}

/// A single register, in the wire format used by the rich stack trace.
#[derive(Serialize, Deserialize, Schema, Clone, PartialEq)]
pub struct WireDebugRegister {
    pub id: WireRegisterId,
    pub dwarf_id: Option<u16>,
    pub value: Option<WireRegisterValue>,
}

impl From<&DebugRegister> for WireDebugRegister {
    fn from(r: &DebugRegister) -> Self {
        WireDebugRegister {
            id: r.core_register.id.into(),
            dwarf_id: r.dwarf_id,
            value: r.value.map(WireRegisterValue::from),
        }
    }
}

/// A stack frame carrying the full per-frame register state plus frame
/// metadata. The server owns the `local_variables`/`static_variables`
/// `VariableCache` trees (keyed by `sessid` + core); the client relays the
/// server-assigned `id`/`locals_reference`/`statics_reference` handles
/// verbatim so subsequent `scopes`/`variables` requests resolve server-side.
#[derive(Serialize, Deserialize, Schema, Clone)]
pub struct RichStackTraceFrame {
    pub function_name: String,
    pub program_counter: WireRegisterValue,
    pub is_inlined: bool,
    pub location: Option<SourceLocation>,
    pub frame_base: Option<u64>,
    pub canonical_frame_address: Option<u64>,
    pub registers: Vec<WireDebugRegister>,
    /// Server-assigned frame id (also the DAP `frameId` and the registers
    /// scope `variablesReference`).
    pub id: u32,
    /// Server-assigned handle for this frame's locals `VariableCache` root.
    pub locals_reference: u32,
    /// Server-assigned handle for the core's static `VariableCache` root.
    pub statics_reference: u32,
}

impl From<StackFrame> for RichStackTraceFrame {
    fn from(frame: StackFrame) -> Self {
        RichStackTraceFrame {
            function_name: frame.function_name.clone(),
            program_counter: WireRegisterValue::from(frame.pc),
            is_inlined: frame.is_inlined,
            location: frame.source_location.as_ref().map(SourceLocation::from),
            frame_base: frame.frame_base,
            canonical_frame_address: frame.canonical_frame_address,
            registers: frame
                .registers
                .0
                .iter()
                .map(WireDebugRegister::from)
                .collect(),
            id: i64::from(frame.id) as u32,
            locals_reference: frame
                .local_variables
                .as_ref()
                .map(|c| i64::from(c.root_variable().variable_key()) as u32)
                .unwrap_or(0),
            statics_reference: 0,
        }
    }
}

#[derive(Serialize, Deserialize, Schema, Clone)]
pub struct RichStackTrace {
    pub core: u32,
    pub frames: Vec<RichStackTraceFrame>,
}

#[derive(Serialize, Deserialize, Schema, Clone)]
pub struct RichStackTraces {
    pub cores: Vec<RichStackTrace>,
}

pub type TakeRichStackTraceResponse = RpcResult<RichStackTraces>;

/// Like [`take_stack_trace`], but the server owns the per-core
/// `local_variables`/`static_variables` `VariableCache` trees (cached in
/// [`crate::rpc::debug_state::ServerDebugState`], keyed by `sessid`). It
/// returns per-frame register state + metadata plus the server-assigned
/// `id`/`locals_reference`/`statics_reference` handles, so an RPC-backed
/// DAP client can resolve `scopes`/`variables` server-side.
pub async fn take_rich_stack_trace(
    ctx: &mut RpcContext,
    _header: VarHeader,
    request: TakeStackTraceRequest,
) -> TakeRichStackTraceResponse {
    // Load (and cache per session) the server-side `DebugInfo`.
    let debug_info = {
        let states = ctx.debug_states();
        let mut guard = states.lock().await;
        if let Some(state) = guard.get(&request.sessid) {
            state.debug_info.clone()
        } else {
            let Some(debug_info) = DebugInfo::from_file(&request.path).ok() else {
                Err("No debug info found.")?
            };
            let state = crate::rpc::debug_state::ServerDebugState::new(debug_info);
            let arc = state.debug_info.clone();
            guard.insert(request.sessid, state);
            arc
        }
    };

    let mut session = ctx.session(request.sessid).await;

    // Per core: unwind, build locals via `get_stackframe_info`, build the
    // static scope cache.
    let cores: Vec<(
        u32,
        Vec<StackFrame>,
        VariableCache,
        Vec<RichStackTraceFrame>,
    )> = session.halted_access(|session| {
        let mut cores = Vec::new();
        for (idx, core_type) in session.list_cores() {
            let mut core = match session.core(idx) {
                Ok(core) => core,
                Err(Error::CoreDisabled(_)) => continue,
                Err(e) => return Err(e),
            };

            let initial_registers = DebugRegisters::from_core(&mut core);
            let exception_interface = exception_handler_for_core(core_type);
            let instruction_set = core.instruction_set().ok();
            let mut stack_frames = debug_info.unwind(
                &mut core,
                initial_registers,
                exception_interface.as_ref(),
                instruction_set,
                request.stack_frame_limit as usize,
            )?;

            let static_variables = debug_info.create_static_scope_cache();
            let statics_ref = static_variables.root_variable().variable_key();

            // Group consecutive frames sharing a register dump (an inlined
            // chain from one `get_stackframe_info` call) and populate
            // `local_variables` for each frame in the group.
            let mut i = 0;
            while i < stack_frames.len() {
                let group_start = i;
                let group_regs = stack_frames[i].registers.clone();
                while i < stack_frames.len() && stack_frames[i].registers == group_regs {
                    i += 1;
                }
                let step_pc: u64 = stack_frames[group_start].pc.try_into().unwrap_or(0);
                let cfa = stack_frames[group_start].canonical_frame_address;
                let mut chain = debug_info
                    .get_stackframe_info(&mut core, step_pc, cfa, &group_regs)
                    .ok()
                    .unwrap_or_default();
                // DIE order is outermost-first; wire order is innermost-first.
                chain.reverse();
                for (offset, frame) in stack_frames[group_start..i].iter_mut().enumerate() {
                    if let Some(cf) = chain.get(offset) {
                        frame.local_variables = cf.local_variables.clone();
                        if frame.source_location.is_none() {
                            frame.source_location = cf.source_location.clone();
                        }
                    }
                }
            }

            let rich_frames = stack_frames
                .iter()
                .map(|f| RichStackTraceFrame {
                    function_name: f.function_name.clone(),
                    program_counter: WireRegisterValue::from(f.pc),
                    is_inlined: f.is_inlined,
                    location: f.source_location.as_ref().map(SourceLocation::from),
                    frame_base: f.frame_base,
                    canonical_frame_address: f.canonical_frame_address,
                    registers: f.registers.0.iter().map(WireDebugRegister::from).collect(),
                    id: i64::from(f.id) as u32,
                    locals_reference: f
                        .local_variables
                        .as_ref()
                        .map(|c| i64::from(c.root_variable().variable_key()) as u32)
                        .unwrap_or(0),
                    statics_reference: i64::from(statics_ref) as u32,
                })
                .collect();

            cores.push((idx as u32, stack_frames, static_variables, rich_frames));
        }
        Ok(cores)
    })?;

    drop(session);

    // Persist the per-core caches server-side for subsequent scopes/variables,
    // and build the wire response from the same data (no extra clones).
    let states = ctx.debug_states();
    let mut guard = states.lock().await;
    let wire_cores: Vec<RichStackTrace> = match guard.get_mut(&request.sessid) {
        Some(state) => cores
            .into_iter()
            .map(|(core, frames, static_variables, rich_frames)| {
                state.store_core(core as usize, frames, Some(static_variables));
                RichStackTrace {
                    core,
                    frames: rich_frames,
                }
            })
            .collect(),
        None => cores
            .into_iter()
            .map(
                |(core, _frames, _static_variables, rich_frames)| RichStackTrace {
                    core,
                    frames: rich_frames,
                },
            )
            .collect(),
    };
    drop(guard);

    Ok(RichStackTraces { cores: wire_cores })
}
