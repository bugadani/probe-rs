use std::fmt::{self, Display, Write as _};

use crate::rpc::{
    Key,
    functions::{
        RpcContext, RpcResult,
        core_ops::{WireRegisterId, WireRegisterValue},
    },
};
use postcard_rpc::header::VarHeader;
use postcard_schema::Schema;
use probe_rs::{Error, Session};
use probe_rs_debug::{DebugInfo, DebugRegister, DebugRegisters, StackFrame, exception_handler_for_core};
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

pub async fn take_stack_trace(
    ctx: &mut RpcContext,
    _header: VarHeader,
    request: TakeStackTraceRequest,
) -> TakeStackTraceResponse {
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

                let mut frames = vec![];
                for frame in stack_frames.into_iter() {
                    frames.push(StackTraceFrame::from(frame));
                }

                cores.push(StackTrace {
                    core: idx as u32,
                    frames,
                });
            }
            Ok(StackTraces { cores })
        })
        .map_err(Into::into)
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
/// metadata, but **without** the `local_variables` cache. The
/// `local_variables` cache is intentionally not serialized (it would
/// require serializing the deep `Variable`/`VariableCache`/gimli type
/// graph). Instead, the DAP client rebuilds locals from its own local
/// `DebugInfo` using [`probe_rs_debug::DebugInfo::get_stackframe_info`].
#[derive(Serialize, Deserialize, Schema, Clone)]
pub struct RichStackTraceFrame {
    pub function_name: String,
    pub program_counter: WireRegisterValue,
    pub is_inlined: bool,
    pub location: Option<SourceLocation>,
    pub frame_base: Option<u64>,
    pub canonical_frame_address: Option<u64>,
    pub registers: Vec<WireDebugRegister>,
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

/// Like [`take_stack_trace`], but returns the full per-frame register
/// state and frame metadata (frame base, CFA, PC, source location) so
/// that an RPC-backed DAP client can reconstruct rich [`StackFrame`]s
/// — including local variables, rebuilt from its own local
/// [`DebugInfo`] — in a single round trip instead of one round trip
/// per memory read during the unwind.
pub async fn take_rich_stack_trace(
    ctx: &mut RpcContext,
    _header: VarHeader,
    request: TakeStackTraceRequest,
) -> TakeRichStackTraceResponse {
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

                let frames = stack_frames
                    .into_iter()
                    .map(RichStackTraceFrame::from)
                    .collect();
                cores.push(RichStackTrace {
                    core: idx as u32,
                    frames,
                });
            }
            Ok(RichStackTraces { cores })
        })
        .map_err(Into::into)
}
