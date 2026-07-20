use std::{fmt::Write, path::Path};

use linkme::distributed_slice;

use crate::cmd::dap_server::{
    DebuggerError,
    debug_adapter::{
        dap::{
            adapter::DebugAdapter,
            repl_commands::{EvalResponse, EvalResult, REPL_COMMANDS, ReplCommand, unimplemented_repl},
            repl_types::ReplCommandArgs,
        },
        protocol::ProtocolAdapter,
    },
    server::session_data::SessionData,
};
use crate::cmd::dap_server::backend::DapBackend;
use crate::rpc::functions::stack_trace::StackTraceFrame;
use crate::util::cli::format_stack_frame;

#[distributed_slice(REPL_COMMANDS)]
static BACKTRACE: ReplCommand = ReplCommand {
    command: "bt",
    requires_target_halted: true,
    sub_commands: &[ReplCommand {
        command: "yaml",
        help_text: "Print all information about the backtrace of the current thread to a local file in YAML format.",
        requires_target_halted: true,
        sub_commands: &[],
        args: &[ReplCommandArgs::Required(
            "path (e.g. my_dir/backtrace.yaml)",
        )],
        handler: unimplemented_repl,
    }],
    help_text: "Print the backtrace of the current thread.",
    args: &[],
    handler: unimplemented_repl,
};

pub(crate) async fn print_backtrace_async<B: DapBackend, P: ProtocolAdapter>(
    debug_adapter: &mut DebugAdapter<P>,
    session_data: &mut SessionData<B>,
    core_index: usize,
    _command_arguments: &str,
) -> EvalResult {
    let colorize = Some(debug_adapter.supports_ansi_styling);

    let Some(cd) = session_data
        .core_data
        .iter()
        .find(|c| c.core_index == core_index)
    else {
        return Err(DebuggerError::UserMessage("No core data".to_string()));
    };

    let mut response_message = String::new();
    for (i, frame) in cd.stack_frames.iter().enumerate() {
        #[allow(clippy::unwrap_used, reason = "Writing to a string is infallible")]
        writeln!(
            &mut response_message,
            "    Frame {}: {}",
            i + 1,
            format_stack_frame(&StackTraceFrame::from(frame), colorize)
        )
        .unwrap();
    }

    Ok(EvalResponse::Message(response_message))
}

pub(crate) async fn save_backtrace_to_yaml_async<B: DapBackend, P: ProtocolAdapter>(
    _debug_adapter: &mut DebugAdapter<P>,
    session_data: &mut SessionData<B>,
    core_index: usize,
    command_arguments: &str,
) -> EvalResult {
    let mut args = command_arguments.split_whitespace();
    let write_to_file = args.next().map(Path::new);

    let Some(cd) = session_data
        .core_data
        .iter()
        .find(|c| c.core_index == core_index)
    else {
        return Err(DebuggerError::UserMessage("No core data".to_string()));
    };

    use insta::_macro_support as insta_yaml;
    let yaml_data = insta_yaml::serialize_value(
        &cd.stack_frames,
        insta_yaml::SerializationFormat::Yaml,
    );

    let response_message = if let Some(location) = write_to_file {
        std::fs::write(location, yaml_data)
            .map_err(|e| DebuggerError::UserMessage(format!("{e:?}")))?;
        format!("Stacktrace successfully stored at {location:?}.")
    } else {
        yaml_data
    };

    Ok(EvalResponse::Message(response_message))
}
