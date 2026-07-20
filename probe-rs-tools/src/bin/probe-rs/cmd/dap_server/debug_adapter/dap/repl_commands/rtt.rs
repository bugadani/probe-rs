use linkme::distributed_slice;

use crate::cmd::dap_server::{
    DebuggerError,
    backend::DapBackend,
    debug_adapter::{
        dap::{
            adapter::DebugAdapter,
            repl_commands::{
                EvalResponse, EvalResult, REPL_COMMANDS, ReplCommand, need_subcommand,
                unimplemented_repl,
            },
            repl_types::ReplCommandArgs,
        },
        protocol::ProtocolAdapter,
    },
    server::session_data::SessionData,
};

#[distributed_slice(REPL_COMMANDS)]
static RTT_COMMANDS: ReplCommand = ReplCommand {
    command: "rtt",
    help_text: "Commands to work with Segger RTT",
    requires_target_halted: false,
    sub_commands: &[ReplCommand {
        command: "write",
        help_text: "Write data to a specific channel.",
        requires_target_halted: false,
        sub_commands: &[],
        args: &[
            ReplCommandArgs::Required("channel_id"),
            ReplCommandArgs::Required("data"),
        ],
        handler: unimplemented_repl,
    }],
    args: &[],
    handler: need_subcommand,
};

pub(crate) async fn rtt_write_async<B: DapBackend, P: ProtocolAdapter>(
    _adapter: &mut DebugAdapter<P>,
    session_data: &mut SessionData<B>,
    core_index: usize,
    input: &str,
) -> EvalResult {
    let (channel_id, data) = input.split_once(' ').ok_or_else(|| {
        DebuggerError::UserMessage("Expected input format: <channel_id> <data>".to_string())
    })?;

    let channel_id = channel_id
        .parse()
        .map_err(|_| DebuggerError::UserMessage("Channel ID must be a number".to_string()))?;

    let Some(cd_idx) = session_data
        .core_data
        .iter()
        .position(|c| c.core_index == core_index)
    else {
        return Err(DebuggerError::UserMessage("No core data".to_string()));
    };

    let mut data = data.to_string();
    data.push('\n');

    let rtt = session_data.core_data[cd_idx]
        .rtt_connection
        .as_mut()
        .ok_or_else(|| DebuggerError::UserMessage("Not connected to RTT".to_string()))?;

    rtt.client
        .write_down_async(&mut session_data.backend, core_index, channel_id, data.into_bytes())
        .await
        .map_err(|e| {
            DebuggerError::UserMessage(format!("Failed to write to channel {channel_id}: {e}"))
        })?;

    Ok(EvalResponse::Message(String::new()))
}
