use std::{fmt::Write as _, path::PathBuf};

use crate::cmd::remote::{
    functions::{Context, EmitterFn, RemoteFunctions},
    Key,
};
use probe_rs::Session;
use probe_rs_debug::{exception_handler_for_core, DebugInfo, DebugRegisters};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct StackTrace {
    pub core: usize,
    pub frames: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct StackTraces {
    pub cores: Vec<StackTrace>,
}

#[derive(Serialize, Deserialize)]
pub(in crate::cmd::remote) struct TakeStackTrace {
    pub path: PathBuf,
    pub sessid: Key<Session>,
}

impl super::RemoteFunction for TakeStackTrace {
    type Message = super::NoMessage;
    type Result = StackTraces;

    async fn run(self, mut ctx: Context<'_, impl EmitterFn>) -> anyhow::Result<StackTraces> {
        let mut session = ctx.session(self.sessid).await;

        let Some(debug_info) = DebugInfo::from_file(&self.path).ok() else {
            anyhow::bail!("No debug info found.");
        };

        session
            .halted_access(|session| {
                let mut cores = Vec::new();
                for (idx, core_type) in session.list_cores() {
                    let mut core = session.core(idx)?;

                    let initial_registers = DebugRegisters::from_core(&mut core);
                    let exception_interface = exception_handler_for_core(core_type);
                    let instruction_set = core.instruction_set().ok();
                    let stack_frames = debug_info
                        .unwind(
                            &mut core,
                            initial_registers,
                            exception_interface.as_ref(),
                            instruction_set,
                        )
                        .unwrap();

                    let mut frame_strings = vec![];
                    for (i, frame) in stack_frames.into_iter().enumerate() {
                        let mut output_stream = String::new();
                        write!(
                            &mut output_stream,
                            "Frame {}: {} @ {}",
                            i, frame.function_name, frame.pc
                        )
                        .unwrap();

                        if frame.is_inlined {
                            write!(&mut output_stream, " inline").unwrap();
                        }
                        writeln!(&mut output_stream).unwrap();

                        if let Some(location) = &frame.source_location {
                            write!(&mut output_stream, "       ").unwrap();
                            write!(&mut output_stream, "{}", location.path.to_path().display())
                                .unwrap();

                            if let Some(line) = location.line {
                                write!(&mut output_stream, ":{line}").unwrap();

                                if let Some(col) = location.column {
                                    let col = match col {
                                        probe_rs_debug::ColumnType::LeftEdge => 1,
                                        probe_rs_debug::ColumnType::Column(c) => c,
                                    };
                                    write!(&mut output_stream, ":{col}").unwrap();
                                }
                            }
                        }

                        frame_strings.push(output_stream);
                    }

                    cores.push(StackTrace {
                        core: idx,
                        frames: frame_strings,
                    });
                }
                Ok(StackTraces { cores })
            })
            .map_err(Into::into)
    }
}

impl From<TakeStackTrace> for RemoteFunctions {
    fn from(func: TakeStackTrace) -> Self {
        RemoteFunctions::StackTrace(func)
    }
}
