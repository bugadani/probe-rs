use crate::cmd::{
    dap_server::{
        DebuggerError,
        backend::DapBackend,
        debug_adapter::{
            dap::{
                adapter::DebugAdapter,
                repl_commands::{EvalResponse, EvalResult, ReplCommand, need_subcommand, unimplemented_repl},
                repl_types::ReplCommandArgs,
            },
            protocol::ProtocolAdapter,
        },
        server::session_data::SessionData,
    },
    run::EmbeddedTestElfInfo,
};

pub(crate) static EMBEDDED_TEST: ReplCommand = ReplCommand {
    command: "test",
    help_text: "Interact with embedded-test test cases",
    requires_target_halted: false,
    sub_commands: &[
        ReplCommand {
            command: "list",
            help_text: "List all test cases.",
            requires_target_halted: false,
            sub_commands: &[],
            args: &[],
            handler: crate::cmd::dap_server::debug_adapter::dap::repl_commands::unimplemented_repl,
        },
        ReplCommand {
            command: "run",
            help_text: "Starts running a test case.",
            requires_target_halted: false,
            sub_commands: &[],
            args: &[ReplCommandArgs::Required("test_name")],
            handler: unimplemented_repl,
        },
    ],
    args: &[],
    handler: need_subcommand,
};

pub(crate) async fn run_test_async<B: DapBackend, P: ProtocolAdapter>(
    adapter: &mut DebugAdapter<P>,
    session_data: &mut SessionData<B>,
    core_index: usize,
    test_name: &str,
) -> EvalResult {
    let Some(test_data) = session_data
        .core_data
        .iter()
        .find(|c| c.core_index == core_index)
        .and_then(|c| c.test_data.downcast_ref::<EmbeddedTestElfInfo>())
    else {
        return Err(DebuggerError::UserMessage(
            "Internal error while trying to access test data".to_string(),
        ));
    };

    let Some(test) = test_data.tests.iter().find(|test| test.name == test_name) else {
        return Err(DebuggerError::UserMessage(format!(
            "Test '{test_name}' not found"
        )));
    };

    let Some(address) = test.address else {
        return Err(DebuggerError::UserMessage(format!(
            "Test '{test_name}' has no address"
        )));
    };

    adapter.reset_and_halt_core_async(session_data, core_index).await?;
    session_data
        .backend
        .kickoff_test(core_index, address as u64)
        .await
        .map_err(DebuggerError::ProbeRs)?;

    if let Some(cd) = session_data
        .core_data
        .iter_mut()
        .find(|c| c.core_index == core_index)
    {
        cd.last_known_status = probe_rs::CoreStatus::Unknown;
    }
    adapter.all_cores_halted = false;

    Ok(EvalResponse::Message(String::new()))
}
