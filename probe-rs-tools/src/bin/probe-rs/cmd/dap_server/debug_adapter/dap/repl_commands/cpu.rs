use crate::cmd::dap_server::{
    debug_adapter::dap::repl_commands::{EvalResult, REPL_COMMANDS, ReplCommand},
    DebuggerError,
};
use anyhow::anyhow;

use linkme::distributed_slice;

#[distributed_slice(REPL_COMMANDS)]
static CONTINUE: ReplCommand = ReplCommand {
    command: "c",
    help_text: "Continue running the program on the target.",
    requires_target_halted: true,
    sub_commands: &[],
    args: &[],
    handler: repl_stub,
};

#[distributed_slice(REPL_COMMANDS)]
static RESET: ReplCommand = ReplCommand {
    command: "reset",
    help_text: "Reset the target",
    requires_target_halted: false,
    sub_commands: &[],
    args: &[],
    handler: repl_stub,
};

#[distributed_slice(REPL_COMMANDS)]
static STEP: ReplCommand = ReplCommand {
    command: "step",
    help_text: "Step one instruction",
    requires_target_halted: true,
    sub_commands: &[],
    args: &[],
    handler: repl_stub,
};

/// Placeholder handler for REPL commands whose logic has been lifted to the
/// async session-level dispatch (`DebugAdapter::dispatch_repl_command`). The
/// `handler` field is still required by `ReplCommand` for the help/completion
/// table; this stub is never invoked for migrated commands.
fn repl_stub(
    _: &mut crate::cmd::dap_server::server::core_data::CoreData,
    _: &str,
    _: &crate::cmd::dap_server::debug_adapter::dap::dap_types::EvaluateArguments,
    _: &mut crate::cmd::dap_server::debug_adapter::dap::adapter::DebugAdapter<
        dyn crate::cmd::dap_server::debug_adapter::protocol::ProtocolAdapter + '_,
    >,
) -> EvalResult {
    Err(DebuggerError::Other(anyhow!(
        "REPL command handled by async dispatch"
    )))
}
