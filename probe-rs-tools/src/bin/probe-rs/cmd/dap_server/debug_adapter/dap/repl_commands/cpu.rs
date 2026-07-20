use crate::cmd::dap_server::{
    backend::DapBackend,
    debug_adapter::{
        dap::{
            adapter::DebugAdapter,
            core_status::DapStatus,
            dap_types::EvaluateArguments,
            repl_commands::{EvalResponse, EvalResult, REPL_COMMANDS, ReplCommand},
        },
        protocol::ProtocolAdapter,
    },
    server::core_data::CoreData,
};
use probe_rs::{CoreStatus, HaltReason};
use probe_rs_debug::SteppingMode;
use linkme::distributed_slice;
use std::future::Future;
use std::pin::Pin;

#[distributed_slice(REPL_COMMANDS)]
static CONTINUE: ReplCommand = ReplCommand {
    command: "c",
    help_text: "Continue running the program on the target.",
    requires_target_halted: true,
    sub_commands: &[],
    args: &[],
    handler: continue_repl,
};

#[distributed_slice(REPL_COMMANDS)]
static RESET: ReplCommand = ReplCommand {
    command: "reset",
    help_text: "Reset the target",
    requires_target_halted: false,
    sub_commands: &[],
    args: &[],
    handler: reset_repl,
};

#[distributed_slice(REPL_COMMANDS)]
static STEP: ReplCommand = ReplCommand {
    command: "step",
    help_text: "Step one instruction",
    requires_target_halted: true,
    sub_commands: &[],
    args: &[],
    handler: step_repl,
};

fn continue_repl<'a>(
    backend: &'a mut dyn DapBackend,
    core_data: &'a mut CoreData,
    _command_arguments: &'a str,
    _evaluate_arguments: &'a EvaluateArguments,
    adapter: &'a mut DebugAdapter<dyn ProtocolAdapter + 'a>,
) -> Pin<Box<dyn Future<Output = EvalResult> + 'a>> {
    Box::pin(async move {
        adapter.continue_impl_async(backend, core_data).await?;
        Ok(EvalResponse::Message(String::new()))
    })
}

fn reset_repl<'a>(
    backend: &'a mut dyn DapBackend,
    core_data: &'a mut CoreData,
    _command_arguments: &'a str,
    _evaluate_arguments: &'a EvaluateArguments,
    adapter: &'a mut DebugAdapter<dyn ProtocolAdapter + 'a>,
) -> Pin<Box<dyn Future<Output = EvalResult> + 'a>> {
    Box::pin(async move {
        adapter.reset_and_halt_core_async(backend, core_data).await?;
        Ok(EvalResponse::Message(String::new()))
    })
}

fn step_repl<'a>(
    backend: &'a mut dyn DapBackend,
    core_data: &'a mut CoreData,
    _command_arguments: &'a str,
    _evaluate_arguments: &'a EvaluateArguments,
    adapter: &'a mut DebugAdapter<dyn ProtocolAdapter + 'a>,
) -> Pin<Box<dyn Future<Output = EvalResult> + 'a>> {
    Box::pin(async move {
        let pc = adapter
            .step_impl_async(SteppingMode::StepInstruction, backend, core_data)
            .await?;
        Ok(EvalResponse::Message(
            CoreStatus::Halted(HaltReason::Request)
                .short_long_status(Some(pc))
                .1,
        ))
    })
}
