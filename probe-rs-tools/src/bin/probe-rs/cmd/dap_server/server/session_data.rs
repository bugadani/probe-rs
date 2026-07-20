use super::{
    configuration::{self, CoreConfig, SessionConfig},
    core_data::{ChannelNames, CoreData, CoreHandle, wire_scan_region},
};
use crate::cmd::dap_server::debug_adapter::dap::dap_types::PromptKind;
use crate::cmd::dap_server::server::debug_rtt;
use crate::util::rtt::client::RttClient;
use crate::util::rtt::{DefmtProcessor, DefmtState, RttDecoder};
use crate::{
    FormatKind,
    cmd::{
        dap_server::{
            DebuggerError,
            backend::rpc::RpcBackend,
            debug_adapter::{
                dap::{
                    adapter::DebugAdapter,
                    core_status::DapStatus,
                    dap_types::{ContinuedEventBody, MessageSeverity, Source, StoppedEventBody},
                    repl_commands::{REPL_COMMANDS, embedded_test::EMBEDDED_TEST},
                },
                protocol::ProtocolAdapter,
            },
        },
        run::EmbeddedTestElfInfo,
    },
    rpc::client::RpcClient,
    util::cli::attach_probe as attach_probe_rpc,
};
use anyhow::{Result, anyhow};
use probe_rs::{
    BreakpointCause, CoreStatus, HaltReason,
    rtt::{ScanRegion, find_rtt_control_block_in_raw_file},
};
use probe_rs_debug::{ColumnType, SourceLocation, VerifiedBreakpoint, debug_info::DebugInfo};
use std::{any::Any, env::set_current_dir, path::Path};
use time::UtcOffset;

use crate::util::rtt::{self, DataFormat};

/// The supported breakpoint types
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum BreakpointType {
    /// A breakpoint was requested using an instruction address, and usually a result of a user requesting a
    /// breakpoint while in a 'disassembly' view.
    InstructionBreakpoint,
    /// A breakpoint that has a Source, and usually a result of a user requesting a breakpoint while in a 'source' view.
    SourceBreakpoint {
        source: Box<Source>,
        location: SourceLocationScope,
    },
}

/// Breakpoint requests refer to a specific `SourceLocation` for a `Source`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SourceLocationScope {
    Specific(SourceLocation),
}

/// Provide the storage and methods to handle various [`BreakpointType`]
#[derive(Clone, Debug)]
pub struct ActiveBreakpoint {
    pub(crate) breakpoint_type: BreakpointType,
    pub(crate) address: u64,
}

/// SessionData is designed to be similar to [probe_rs::Session], in as much that it provides handles to the [CoreHandle] instances for each of the available [probe_rs::Core] involved in the debug session.
/// To get access to the [CoreHandle] for a specific [probe_rs::Core], the
/// TODO: Adjust [SessionConfig] to allow multiple cores (and if appropriate, their binaries) to be specified.
///
/// The DAP server drives the target through an [`RpcBackend`]: even local
/// sessions run through the in-process RPC server, so the debugger never
/// touches a [`probe_rs::Session`] directly.
pub(crate) struct SessionData {
    pub(crate) backend: RpcBackend,
    /// [SessionData] will manage one [CoreData] per target core, that is also present in [SessionConfig::core_configs]
    pub(crate) core_data: Vec<CoreData>,

    /// Offset used for RTC timestamps
    ///
    /// Getting the offset can fail, so it's better to store it.
    timestamp_offset: UtcOffset,
}

impl SessionData {
    /// Build a [`SessionData`] backed by an [`RpcBackend`] against the
    /// supplied [`RpcClient`].
    ///
    /// The server is expected to have its chip registry populated (either
    /// through the builtin database or through a prior
    /// `client.load_chip_family`). The `config.chip` field is required: it
    /// is used both to drive the remote `probe/attach` RPC and to look up
    /// the matching [`probe_rs::Target`] description locally so that the
    /// DAP server can answer memory-map / RTT / SVD queries without extra
    /// round trips.
    pub(crate) async fn new_remote(
        client: &RpcClient,
        config: &mut configuration::SessionConfig,
        timestamp_offset: UtcOffset,
    ) -> Result<Self, DebuggerError> {
        use crate::cmd::dap_server::backend::rpc::CorePerAttachInfo;

        let chip_name = config
            .chip
            .clone()
            .ok_or_else(|| anyhow!("A chip name is required when debugging over RPC."))?;

        // Reuse the shared CLI helper: it uploads any user-supplied chip
        // description, selects a probe, and performs the `probe/attach` RPC.
        let probe_options = config.probe_options();
        let session = attach_probe_rpc(client, probe_options, None, false).await?;
        let sessid = session.session_key();

        // The client has a local registry mirror: look up the target so we
        // can serve memory-map / introspection locally. This assumes the
        // client has loaded the same chip families the server has, which is
        // how [`attach_probe_rpc`] already works.
        let target = {
            let registry = client.registry().await;
            registry.get_target_by_name(&chip_name).map_err(|e| {
                DebuggerError::Other(anyhow!(
                    "Failed to resolve chip `{chip_name}` in the local registry: {e}"
                ))
            })?
        };

        apply_session_cwd(config)?;

        let cores: Vec<(usize, probe_rs::CoreType)> = target
            .cores
            .iter()
            .enumerate()
            .map(|(idx, core)| (idx, core.core_type))
            .collect();

        // Derive per-core defaults from the core type. Endianness is
        // assumed little-endian (true for all currently supported cores);
        // FPU support is assumed absent until the user needs it — the
        // correct way to populate these fields is with an explicit RPC
        // query once the core is halted, which we do not yet issue here.
        let per_core: Vec<CorePerAttachInfo> = cores
            .iter()
            .map(|(_, core_type)| CorePerAttachInfo {
                architecture: core_type.architecture(),
                fpu_support: false,
                fp_register_count: None,
            })
            .collect();

        let mut backend = RpcBackend::new(
            tokio::runtime::Handle::current(),
            client.clone(),
            sessid,
            target,
            cores,
            per_core,
        );

        let core_data_vec = initialize_core_data(&mut backend, config)?;
        for core_config in config.core_configs.iter() {
            backend
                .apply_vector_catch(core_config.core_index, core_config)
                .await?;
        }

        // Eagerly populate the server-side `DebugInfo` from the program
        // binary so server-side consumers (notably `disassemble`) can resolve
        // source locations before the first halt — mirroring the local
        // backend, which loads `DebugInfo` at session start. Use the first
        // configured core's binary (the RPC server caches one `DebugInfo` per
        // session, matching `take_rich_stack_trace`).
        if let Some(Some(path)) = config
            .core_configs
            .first()
            .map(|c| c.program_binary.as_deref())
        {
            backend
                .session_interface()
                .load_debug_info(path.to_path_buf())
                .await
                .map_err(|e| DebuggerError::Other(anyhow::anyhow!("Failed to load debug info: {e}")))?;
        }

        let mut this = SessionData {
            backend,
            core_data: core_data_vec,
            timestamp_offset,
        };

        this.load_rtt_location(config)?;

        Ok(this)
    }
}

impl SessionData {
    pub(crate) fn load_rtt_location(
        &mut self,
        config: &configuration::SessionConfig,
    ) -> Result<(), DebuggerError> {
        // Filter `CoreConfig` entries based on those that match an actual core on the target probe.
        let valid_core_configs = config.core_configs.iter().filter(|&core_config| {
            self.backend
                .list_cores()
                .iter()
                .any(|(target_core_index, _)| *target_core_index == core_config.core_index)
        });

        let image_format = config
            .flashing_config
            .format_options
            .binary_format
            .resolve(self.backend.target());

        for core_configuration in valid_core_configs {
            let Some(core_data) = self
                .core_data
                .iter_mut()
                .find(|core_data| core_data.core_index == core_configuration.core_index)
            else {
                continue;
            };

            core_data.rtt_scan_ranges = match core_configuration.program_binary.as_ref() {
                Some(program_binary)
                    if matches!(image_format, FormatKind::Elf | FormatKind::Idf) =>
                {
                    let elf = std::fs::read(program_binary)
                        .map_err(|error| anyhow!("Error attempting to attach to RTT: {error}"))?;

                    if let Ok(Some(addr)) = find_rtt_control_block_in_raw_file(&elf) {
                        ScanRegion::Exact(addr)
                    } else {
                        // Do not scan the memory for the control block.
                        ScanRegion::Ranges(vec![])
                    }
                }
                _ => ScanRegion::Ranges(vec![]),
            };
        }

        Ok(())
    }

    /// Clear stale RTT control blocks for all cores.
    ///
    /// This should be called while the core is halted, before a reset, to wipe
    /// stale RTT data from a previous debug session. After reset, the firmware
    /// startup code will reinitialize the block from `.data`.
    pub(crate) async fn clear_rtt_blocks(&mut self) -> Result<(), DebuggerError> {
        for core_data in self.core_data.iter() {
            self.backend
                .clear_rtt_blocks(core_data.core_index, &core_data.rtt_scan_ranges)
                .await
                .map_err(DebuggerError::ProbeRs)?;
        }
        Ok(())
    }

    /// Recompute source breakpoint addresses after a restart that flashed a
    /// new binary: re-verify each cached source breakpoint against the
    /// (reloaded) debug info and re-set the hardware breakpoints, since the
    /// address of a source location may have shifted.
    pub(crate) async fn recompute_breakpoints(
        &mut self,
        core_index: usize,
    ) -> Result<(), DebuggerError> {
        let (old_addrs, to_set) = {
            let Some(core_data) = self
                .core_data
                .iter_mut()
                .find(|cd| cd.core_index == core_index)
            else {
                return Ok(());
            };
            let Some(debug_info) = core_data.debug_info.as_ref() else {
                return Ok(());
            };
            let mut old_addrs: Vec<u64> = Vec::new();
            let mut to_set: Vec<(u64, Box<Source>, SourceLocation)> = Vec::new();
            for bp in &core_data.breakpoints {
                let BreakpointType::SourceBreakpoint {
                    source,
                    location: SourceLocationScope::Specific(loc),
                } = &bp.breakpoint_type
                else {
                    continue;
                };
                old_addrs.push(bp.address);
                let column = loc.column.map(|col| match col {
                    ColumnType::LeftEdge => 0_u64,
                    ColumnType::Column(c) => c,
                });
                match debug_info.get_breakpoint_location(
                    loc.path.to_path(),
                    loc.line.unwrap_or(0),
                    column,
                ) {
                    Ok(VerifiedBreakpoint {
                        address,
                        source_location,
                    }) => to_set.push((address, source.clone(), source_location)),
                    Err(e) => {
                        return Err(DebuggerError::Other(anyhow!(
                            "Failed to recompute breakpoint at {loc:?} in {source:?}. Error: {e:?}"
                        )));
                    }
                }
            }
            (old_addrs, to_set)
        };
        if old_addrs.is_empty() {
            return Ok(());
        }
        self.backend
            .clear_hw_breakpoints(core_index, old_addrs)
            .await
            .map_err(DebuggerError::ProbeRs)?;
        if let Some(core_data) = self
            .core_data
            .iter_mut()
            .find(|cd| cd.core_index == core_index)
        {
            core_data.breakpoints.retain(|bp| {
                !matches!(&bp.breakpoint_type, BreakpointType::SourceBreakpoint { .. })
            });
        }
        let set_addrs: Vec<u64> = to_set.iter().map(|(a, _, _)| *a).collect();
        let set_results = self
            .backend
            .set_hw_breakpoints(core_index, set_addrs)
            .await
            .map_err(DebuggerError::ProbeRs)?;
        if let Some(core_data) = self
            .core_data
            .iter_mut()
            .find(|cd| cd.core_index == core_index)
        {
            for (i, (addr, source, loc)) in to_set.into_iter().enumerate() {
                if set_results.get(i).copied().unwrap_or(false) {
                    core_data.breakpoints.push(ActiveBreakpoint {
                        breakpoint_type: BreakpointType::SourceBreakpoint {
                            source,
                            location: SourceLocationScope::Specific(loc),
                        },
                        address: addr,
                    });
                }
            }
        }
        Ok(())
    }

    /// Reload the a specific core's debug info from the binary file.
    pub(crate) fn load_debug_info_for_core(
        &mut self,
        core_configuration: &CoreConfig,
    ) -> Result<(), DebuggerError> {
        if let Some(core_data) = self
            .core_data
            .iter_mut()
            .find(|core_data| core_data.core_index == core_configuration.core_index)
        {
            core_data.debug_info = debug_info_from_binary(core_configuration)?;
            Ok(())
        } else {
            Err(DebuggerError::UnableToOpenProbe(Some(
                "No core at the specified index.",
            )))
        }
    }

    /// Do a 'light weight' (just get references to existing data structures) attach to the core and return relevant debug data.
    pub(crate) fn attach_core(
        &mut self,
        core_index: usize,
    ) -> Result<CoreHandle<'_>, DebuggerError> {
        if let (Ok(target_core), Some(core_data)) = (
            self.backend.core(core_index),
            self.core_data
                .iter_mut()
                .find(|core_data| core_data.core_index == core_index),
        ) {
            Ok(CoreHandle {
                core: target_core,
                core_data,
            })
        } else {
            Err(DebuggerError::UnableToOpenProbe(Some(
                "No core at the specified index.",
            )))
        }
    }

    /// Emit the `stopped` event for a halted core without a live `Core`: the
    /// PC is read via `backend.program_counter_id` + `backend.read_core_reg`
    /// (one round trip for the register read; the PC id is cached).
    async fn notify_halted<P: ProtocolAdapter>(
        &mut self,
        debug_adapter: &mut DebugAdapter<P>,
        cd_idx: usize,
        status: CoreStatus,
    ) -> Result<(), DebuggerError> {
        let core_index = self.core_data[cd_idx].core_index;
        let program_counter = match self.backend.program_counter_id(core_index).await {
            Ok(id) => self
                .backend
                .read_core_reg(core_index, id)
                .await
                .ok()
                .and_then(|v| v.try_into().ok()),
            Err(_) => None,
        };
        let (reason, description) = status.short_long_status(program_counter);
        let event_body = Some(StoppedEventBody {
            reason: reason.to_string(),
            description: Some(description),
            thread_id: Some(core_index as i64),
            preserve_focus_hint: Some(false),
            text: None,
            all_threads_stopped: Some(debug_adapter.all_cores_halted),
            hit_breakpoint_ids: None,
        });
        debug_adapter.send_event("stopped", event_body)?;
        tracing::trace!("Notified DAP client that the core halted: {:?}", status);
        Ok(())
    }

    /// Update `last_known_status` and emit the appropriate DAP event for a
    /// status transition, without a live `Core`. Semihosting halts are
    /// skipped here (the poll loop handles them separately) and the PC for
    /// the `stopped` event is read via [`Self::notify_halted`].
    async fn process_core_status<P: ProtocolAdapter>(
        &mut self,
        debug_adapter: &mut DebugAdapter<P>,
        cd_idx: usize,
        status: CoreStatus,
    ) -> Result<CoreStatus, DebuggerError> {
        if status == self.core_data[cd_idx].last_known_status {
            return Ok(status);
        }
        self.core_data[cd_idx].last_known_status = status;

        match status {
            CoreStatus::Running | CoreStatus::Sleeping => {
                let event_body = Some(ContinuedEventBody {
                    all_threads_continued: Some(true),
                    thread_id: cd_idx as i64,
                });
                debug_adapter.send_event("continued", event_body)?;
                tracing::trace!("Notified DAP client that the core continued: {:?}", status);
            }
            CoreStatus::Halted(HaltReason::Step) => {}
            CoreStatus::Halted(HaltReason::Breakpoint(BreakpointCause::Semihosting(_))) => {}
            CoreStatus::Halted(_) => self.notify_halted(debug_adapter, cd_idx, status).await?,
            CoreStatus::LockedUp => {
                debug_adapter.show_message(
                    MessageSeverity::Warning,
                    format!("Core {} is in locked up state", cd_idx),
                );
                self.notify_halted(debug_adapter, cd_idx, status).await?;
            }
            CoreStatus::Unknown => {
                let error =
                    DebuggerError::Other(anyhow!("Unknown Device status received from Probe-rs"));
                debug_adapter.show_error_message(&error)?;
                return Err(error);
            }
        }
        Ok(status)
    }

    /// Attach to the target's RTT interface without a `CoreHandle`: the local
    /// path acquires a `Core` via `self.backend.core()` (disjoint from
    /// `self.core_data`); the remote path drives the server-side `RttClient`
    /// via the cached `RttRemoteSeed` and needs no `Core`.
    async fn attach_to_rtt<P: ProtocolAdapter>(
        &mut self,
        debug_adapter: &mut DebugAdapter<P>,
        cd_idx: usize,
        program_binary: Option<&Path>,
        rtt_config: &rtt::RttConfig,
        timestamp_offset: UtcOffset,
    ) -> Result<()> {
        if self.core_data[cd_idx].rtt_connection.is_some() {
            return Ok(());
        }

        let core_index = self.core_data[cd_idx].core_index;

        let mut defmt_data = None;
        let use_auto_formats = rtt_config.channels.is_empty();

        let mut build_up_channel = |debug_adapter: &mut DebugAdapter<P>,
                                    number: u32,
                                    channel_name: &str|
         -> Result<debug_rtt::DebuggerRttChannel> {
            let mut channel_config = rtt_config.channel_config(number).clone();

            if use_auto_formats {
                channel_config.data_format = if channel_name == "defmt" {
                    DataFormat::Defmt
                } else {
                    DataFormat::String
                };
            }

            let show_timestamps = channel_config.show_timestamps;
            let show_location = channel_config.show_location;
            let log_format = channel_config.log_format.clone();

            let channel_data_format = match channel_config.data_format {
                DataFormat::String => RttDecoder::String {
                    timestamp_offset: Some(timestamp_offset),
                    last_line_done: false,
                    show_timestamps,
                },
                DataFormat::BinaryLE => RttDecoder::BinaryLE,
                DataFormat::Defmt => {
                    let defmt_state = if let Some(data) = defmt_data.as_ref() {
                        data
                    } else if let Some(program_binary) = program_binary {
                        let elf = std::fs::read(program_binary).map_err(|error| {
                            anyhow!("Error attempting to attach to RTT: {error}")
                        })?;
                        defmt_data.insert(DefmtState::try_from_bytes(&elf)?)
                    } else {
                        defmt_data.insert(None)
                    };

                    match defmt_state {
                        Some(defmt_state) => RttDecoder::Defmt {
                            processor: DefmtProcessor::new(
                                defmt_state.clone(),
                                show_timestamps,
                                show_location,
                                log_format.as_deref(),
                            ),
                        },
                        None => RttDecoder::BinaryLE,
                    }
                }
            };

            let data_format = DataFormat::from(&channel_data_format);

            debug_adapter.rtt_window(number, channel_name.to_string(), data_format);

            Ok(debug_rtt::DebuggerRttChannel {
                channel_number: number,
                has_client_window: false,
                channel_data_format,
            })
        };

        let (client, up_channels, down_channels): (
            debug_rtt::RttClientHandle,
            ChannelNames,
            ChannelNames,
        ) = if let Some(seed) = self.core_data[cd_idx].rtt_remote_seed.clone() {
            let rtt_key = if let Some(k) = self.core_data[cd_idx].rtt_remote_handle {
                k
            } else {
                let wire_scan = wire_scan_region(&self.core_data[cd_idx].rtt_scan_ranges);
                let data = seed
                    .session
                    .create_rtt_client(
                        wire_scan,
                        rtt_config.channels.clone(),
                        rtt_config.default_config.clone(),
                    )
                    .await
                    .map_err(|e| anyhow!("Failed to create remote RTT client: {e}"))?;
                self.core_data[cd_idx].rtt_remote_handle = Some(data.handle);
                data.handle
            };

            let channels = seed
                .session
                .get_rtt_channels(rtt_key)
                .await
                .map_err(|e| anyhow!("Failed to query remote RTT channels: {e}"))?;

            if channels.up.is_empty() && channels.down.is_empty() {
                return Ok(());
            }

            let up = channels
                .up
                .into_iter()
                .map(|m| (m.number, m.name))
                .collect();
            let down = channels
                .down
                .into_iter()
                .map(|m| (m.number, m.name))
                .collect();

            let handle = debug_rtt::RttClientHandle::Remote(debug_rtt::RemoteRttClient::new(
                seed.session.clone(),
                rtt_key,
            ));

            (handle, up, down)
        } else {
            let mut core = self.backend.core(core_index)?;
            let core_data = &mut self.core_data[cd_idx];
            let client = if let Some(client) = core_data.rtt_client.as_mut() {
                client
            } else {
                core_data.rtt_client.insert(RttClient::new(
                    rtt_config.clone(),
                    core_data.rtt_scan_ranges.clone(),
                    core.target(),
                ))
            };

            if client.core_id() != core_index {
                return Ok(());
            }

            let Ok(true) = client.try_attach(&mut core) else {
                return Ok(());
            };

            let Some(client) = core_data.rtt_client.take() else {
                return Ok(());
            };

            let up = client
                .up_channels()
                .iter()
                .map(|c| (c.number(), c.channel_name()))
                .collect();
            let down = client
                .down_channels()
                .iter()
                .map(|c| (c.number(), c.channel_name()))
                .collect();

            let handle = debug_rtt::RttClientHandle::Local(client);
            (handle, up, down)
        };

        let mut debugger_rtt_channels = vec![];
        for (number, name) in &up_channels {
            debugger_rtt_channels.push(build_up_channel(debug_adapter, *number, name)?);
        }

        for (number, name) in &down_channels {
            debug_adapter.open_prompt(PromptKind::Rtt, name, *number);
        }

        self.core_data[cd_idx].rtt_connection = Some(debug_rtt::RttConnection {
            client,
            debugger_rtt_channels,
        });

        Ok(())
    }

    /// The target has no way of notifying the debug adapter when things changes, so we have to constantly poll it to determine:
    /// - Whether the target cores are running, and what their actual status is.
    /// - Whether the target cores have data in their RTT buffers that we need to read and pass to the client.
    ///
    /// To optimize this polling process while also optimizing the reading of RTT data, we apply a couple of principles:
    /// 1. Sleep (nap for a short duration) between polling each target core, but:
    /// - Only sleep IF the core's status hasn't changed AND there was no RTT data in the last poll.
    /// - Otherwise move on without delay, to keep things flowing as fast as possible.
    /// - The justification is that any client side CPU used to keep polling is a small price to pay for maximum throughput of debug requests and RTT from the probe.
    /// 2. Check all target cores to ensure they have a configured and initialized RTT connections and if they do, process the RTT data.
    /// - To keep things efficient, the polling of RTT data is done only when we expect there to be data available.
    /// - We check for RTT only when the core has an RTT connection configured, and one of the following is true:
    ///   - While the core is NOT halted, because core processing can generate new data at any time.
    ///   - The first time we have entered halted status, to ensure the buffers are drained. After that, for as long as we remain in halted state, we don't need to check RTT again.
    ///
    /// Return a boolean indicating whether we should consider a short delay before the next poll.
    #[tracing::instrument(level = "trace", skip_all)]
    pub(crate) async fn poll_cores<P: ProtocolAdapter>(
        &mut self,
        session_config: &SessionConfig,
        debug_adapter: &mut DebugAdapter<P>,
    ) -> Result<bool, DebuggerError> {
        // By default, we will have a small delay between polls, and will disable it if
        // we know the last poll returned data, on the assumption that there might be at least one more batch of data.
        let mut suggest_delay_required = true;

        let timestamp_offset = self.timestamp_offset;

        let cores_halted_previously = debug_adapter.all_cores_halted;

        // Always set `all_cores_halted` to true, until one core is found to be running.
        debug_adapter.all_cores_halted = true;

        // Cores that transitioned to halted during this poll and need their
        // stack frames rebuilt. We defer the actual unwind to *after* the
        // per-core loop so that `&mut self.backend` is free (the loop body
        // holds a `CoreHandle` borrowing the backend for status polling).
        let mut needs_unwind: Vec<usize> = Vec::new();

        for core_config in session_config.core_configs.iter() {
            // Fetch status via the backend before `attach_core` so the
            // `&mut self.backend` borrow is released before `attach_core`
            // reborrows it.
            let current_core_status = if debug_adapter.configuration_is_done() {
                match self.backend.status(core_config.core_index).await {
                    Ok(status) => status,
                    Err(error) => {
                        let err = DebuggerError::from(error);
                        let _ = debug_adapter.show_error_message(&err);
                        if let Some(cd) = self
                            .core_data
                            .iter_mut()
                            .find(|c| c.core_index == core_config.core_index)
                        {
                            cd.last_known_status = CoreStatus::Unknown;
                        }
                        return Err(err);
                    }
                }
            } else {
                CoreStatus::Unknown
            };

            let core_index = core_config.core_index;
            let Some(cd_idx) = self
                .core_data
                .iter()
                .position(|c| c.core_index == core_index)
            else {
                tracing::debug!("No core data for core #{core_index}; cannot poll.");
                continue;
            };
            let previous_core_status = self.core_data[cd_idx].last_known_status;

            let mut current_core_status = self
                .process_core_status(debug_adapter, cd_idx, current_core_status)
                .await
                .inspect_err(|error| {
                    let _ = debug_adapter.show_error_message(error);
                })?;

            let semihosting_command = match current_core_status {
                CoreStatus::Halted(HaltReason::Breakpoint(BreakpointCause::Semihosting(c))) => {
                    Some(c)
                }
                _ => None,
            };

            // RTT polling and semihosting handling need a live `Core`. Acquire
            // a short-lived `CoreHandle` in a scoped block so the
            // `&mut self.backend` borrow is released before the deferred
            // unwind / next iteration.
            let rtt_enabled = core_config.rtt_config.enabled;

            // RTT polling: local path reads target memory via a `Core`,
            // remote path drives the server-side `RttClient`. The poll uses
            // disjoint borrows of `self.backend` and `self.core_data`, so no
            // long-lived `CoreHandle` is needed. `attach_to_rtt` still needs
            // a `CoreHandle` (it reads target metadata), so it runs in a
            // scoped block.
            if rtt_enabled {
                if self.core_data[cd_idx].rtt_connection.is_some() {
                    let is_remote = self.core_data[cd_idx]
                        .rtt_connection
                        .as_ref()
                        .is_some_and(|c| c.is_remote());
                    let had_data = if is_remote {
                        match self.core_data[cd_idx].rtt_connection.as_mut() {
                            Some(core_rtt) => core_rtt.process_rtt_data_remote(debug_adapter).await,
                            None => false,
                        }
                    } else {
                        let mut core = self.backend.core(core_index)?;
                        match self.core_data[cd_idx].rtt_connection.as_mut() {
                            Some(core_rtt) => {
                                core_rtt.process_rtt_data(debug_adapter, &mut core).await
                            }
                            None => false,
                        }
                    };
                    if had_data {
                        suggest_delay_required = false;
                    }
                } else if debug_adapter.configuration_is_done()
                    && let Err(error) = self
                        .attach_to_rtt(
                            debug_adapter,
                            cd_idx,
                            core_config.program_binary.as_deref(),
                            &core_config.rtt_config,
                            timestamp_offset,
                        )
                        .await
                {
                    debug_adapter
                        .show_error_message(&DebuggerError::Other(error))
                        .ok();
                }
            }

            // Semihosting handling runs via `RpcBackend::handle_semihosting`
            // (local: drives the live `Core` + client-owned state; RPC:
            // server-owned state). The backend returns UI events (RTT
            // window open, console/RTT output) that we replay on the DAP
            // adapter here.
            if let Some(_command) = semihosting_command {
                let result = self
                    .backend
                    .handle_semihosting(core_index)
                    .await?;
                for event in result.events {
                    match event {
                        crate::cmd::dap_server::backend::SemihostingUiEvent::RttWindow {
                            handle,
                            path,
                            format,
                        } => {
                            debug_adapter.rtt_window(handle, path, format);
                        }
                        crate::cmd::dap_server::backend::SemihostingUiEvent::LogToConsole(msg) => {
                            debug_adapter.log_to_console(msg);
                        }
                        crate::cmd::dap_server::backend::SemihostingUiEvent::RttOutput {
                            handle,
                            data,
                        } => {
                            debug_adapter.rtt_output(handle, data);
                        }
                    }
                }
                current_core_status = result.status;

                if current_core_status.is_halted() {
                    // poll_core did not notify about the halt, so we need to do it manually.
                    self.notify_halted(debug_adapter, cd_idx, current_core_status)
                        .await?;
                } else {
                    // If the semihosting command was handled, we do not need to suggest a delay.
                    suggest_delay_required = false;
                }
                self.core_data[cd_idx].last_known_status = current_core_status;
            }

            // Non-semihosting status change: log the new status (PC read via
            // the backend).
            if current_core_status != previous_core_status && semihosting_command.is_none() {
                let pc = if current_core_status.is_halted() {
                    match self.backend.program_counter_id(core_index).await {
                        Ok(id) => self
                            .backend
                            .read_core_reg(core_index, id)
                            .await
                            .ok()
                            .and_then(|v| v.try_into().ok()),
                        Err(_) => None,
                    }
                } else {
                    None
                };
                debug_adapter.log_to_console(current_core_status.short_long_status(pc).1);
            }

            // If the core is running, we set the flag to indicate that at least one core is not halted.
            // By setting it here, we ensure that RTT will be checked at least once after the core has halted.
            if !current_core_status.is_halted() {
                debug_adapter.all_cores_halted = false;
            } else if !cores_halted_previously
                && let Some(debug_info) = self.core_data[cd_idx].debug_info.as_ref()
            {
                // If currently halted, and was previously running
                // update the stack frames
                let _stackframe_span = tracing::debug_span!("Update Stack Frames").entered();
                tracing::debug!("Updating the stack frame data for core #{}", core_index);

                if self.core_data[cd_idx].static_variables.is_none() {
                    self.core_data[cd_idx].static_variables =
                        Some(debug_info.create_static_scope_cache());
                }

                // Defer the unwind: `RpcBackend::unwind_stack` borrows
                // `&mut self.backend`, which is free now (the scoped
                // `CoreHandle` is dropped).
                needs_unwind.push(core_index);
            }
        }

        // Deferred unwinds (per-core `CoreHandle`s are now dropped). Resolve
        // each fully before touching `self.core_data` so no borrow is held
        // across the `.await`.
        for &core_index in &needs_unwind {
            let program_binary = session_config
                .core_configs
                .iter()
                .find(|c| c.core_index == core_index)
                .and_then(|c| c.program_binary.as_deref());

            let Some(debug_info) = self
                .core_data
                .iter()
                .find(|cd| cd.core_index == core_index)
                .and_then(|cd| cd.debug_info.as_ref())
            else {
                continue;
            };

            let frames = self
                .backend
                .unwind_stack(core_index, program_binary, debug_info, 500)
                .await
                .map_err(DebuggerError::ProbeRs)?;

            if let Some(core_data) = self
                .core_data
                .iter_mut()
                .find(|cd| cd.core_index == core_index)
            {
                core_data.stack_frames = frames;
            }
        }
        Ok(suggest_delay_required)
    }

    pub(crate) async fn clean_up(
        &mut self,
        session_config: &SessionConfig,
    ) -> Result<(), DebuggerError> {
        for core_config in session_config.core_configs.iter() {
            if core_config.rtt_config.enabled {
                let Some(cd_idx) = self
                    .core_data
                    .iter()
                    .position(|c| c.core_index == core_config.core_index)
                else {
                    continue;
                };
                if self.core_data[cd_idx].rtt_connection.is_some() {
                    let is_remote = self.core_data[cd_idx]
                        .rtt_connection
                        .as_ref()
                        .is_some_and(|c| c.is_remote());
                    if is_remote {
                        if let Some(core_rtt) = self.core_data[cd_idx].rtt_connection.as_mut() {
                            core_rtt.clean_up_async(None).await?;
                        }
                    } else {
                        let mut core = self.backend.core(core_config.core_index)?;
                        if let Some(core_rtt) = self.core_data[cd_idx].rtt_connection.as_mut() {
                            core_rtt.clean_up_async(Some(&mut core)).await?;
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

fn debug_info_from_binary(core_configuration: &CoreConfig) -> anyhow::Result<Option<DebugInfo>> {
    let Some(ref binary_path) = core_configuration.program_binary else {
        return Ok(None);
    };

    DebugInfo::from_file(binary_path)
        .map_err(|error| anyhow!(error))
        .map(Some)
}

/// Apply the session config's requested working directory if one was
/// supplied. Shared between the local and RPC attach paths.
///
/// Skipped in `remote_server_mode`: there `cwd` is a path on the *client's* filesystem
/// (kept around as a display-only string for log messages), so calling `set_current_dir`
/// with it on the server would fail. Relative-path resolution against `cwd` does not
/// apply in remote mode — every client-supplied file path arrives either already absolute,
/// or materialized by [`SessionConfig::materialize_uploaded_files`] to an absolute temp
/// path — so the working directory does not need to change for downstream code to work.
fn apply_session_cwd(config: &configuration::SessionConfig) -> Result<(), DebuggerError> {
    if !config.remote_server_mode
        && let Some(new_cwd) = config.cwd.clone()
    {
        set_current_dir(new_cwd.as_path()).map_err(|err| {
            anyhow!("Failed to set current working directory to: {new_cwd:?}, {err:?}")
        })?;
    }
    Ok(())
}

/// Build the per-core [`CoreData`] record the DAP server uses to track a
/// debug session. Called once per configured core by [`initialize_core_data`].
fn build_core_data(
    core_configuration: &CoreConfig,
    target_name: &str,
) -> Result<CoreData, DebuggerError> {
    // Load debug info first, which also validates the accessibility of the elf.
    let debug_info = debug_info_from_binary(core_configuration)?;

    let mut repl_commands = REPL_COMMANDS.to_vec();
    let mut test_data: Box<dyn Any> = Box::new(());
    if let Some(path_to_elf) = core_configuration.program_binary.as_deref()
        && let Some(elf_info) = EmbeddedTestElfInfo::from_elf(path_to_elf)?
    {
        tracing::debug!("Embedded Test Metadata: {:?}", elf_info);
        if elf_info.version != 1 {
            tracing::info!("Detected unsupported embedded-test version in ELF file.");
        } else {
            tracing::info!(
                "Detected embedded-test in ELF file. Adding `test` command to Debug Console."
            );

            repl_commands.push(EMBEDDED_TEST);
            test_data = Box::new(elf_info);
        }
    }

    Ok(CoreData {
        core_index: core_configuration.core_index,
        last_known_status: CoreStatus::Unknown,
        target_name: format!("{}-{}", core_configuration.core_index, target_name),
        debug_info,
        static_variables: None,
        core_peripherals: None,
        stack_frames: vec![],
        breakpoints: vec![],
        rtt_scan_ranges: ScanRegion::Ranges(vec![]),
        rtt_connection: None,
        rtt_client: None,
        rtt_remote_seed: None,
        rtt_remote_handle: None,
        repl_commands,
        test_data,
    })
}

/// Run the shared "post-attach" core initialization: enforce the
/// single-core invariant, filter config entries down to cores that
/// actually exist on the target, apply vector-catch settings, and build
/// the [`CoreData`] vector.
fn initialize_core_data(
    backend: &mut RpcBackend,
    config: &configuration::SessionConfig,
) -> Result<Vec<CoreData>, DebuggerError> {
    if config.core_configs.len() != 1 {
        // TODO: For multi-core, allow > 1.
        return Err(DebuggerError::Other(anyhow!(
            "probe-rs-debugger requires that one, and only one, core be configured for debugging."
        )));
    }

    let available_cores = backend.list_cores();
    let target_name = backend.target().name.clone();

    let valid_core_configs = config.core_configs.iter().filter(|&core_config| {
        available_cores
            .iter()
            .any(|(target_core_index, _)| *target_core_index == core_config.core_index)
    });

    let mut core_data_vec = vec![];
    for core_configuration in valid_core_configs {
        let mut core_data = build_core_data(core_configuration, &target_name)?;
        core_data.rtt_remote_seed = backend.rtt_remote_seed();
        core_data_vec.push(core_data);
    }
    Ok(core_data_vec)
}
