use std::borrow::Cow;

use crate::util::rtt::client::RttClient;
use crate::{
    cmd::dap_server::{
        DebuggerError,
        debug_adapter::{dap::adapter::*, protocol::ProtocolAdapter},
    },
    rpc::{Key, client::SessionInterface},
    util::rtt::RttDecoder,
};
use anyhow::anyhow;
use probe_rs::{Core, rtt::Error as RttError};

/// Per-channel result of a batched [`RttClientHandle::poll_channels`] call.
/// The data is always owned (`Cow::Owned`), so the results do not borrow from
/// the RTT client and can be passed to a separate method on `RttConnection`.
pub(crate) type ChannelPollResults = Vec<(u32, Result<Cow<'static, [u8]>, RttError>)>;

/// The RTT client the DAP server drives. `Local` reads the target directly
/// through a [`Core`]; `Remote` drives the server-side `RttClient` so each
/// poll is a single `rtt/poll_up` round trip.
pub enum RttClientHandle {
    Local(RttClient),
    Remote(RemoteRttClient),
}

/// Handle to the server-side [`RttClient`].
pub struct RemoteRttClient {
    session: SessionInterface,
    rtt_client: Key<RttClient>,
}

impl RemoteRttClient {
    pub(crate) fn new(session: SessionInterface, rtt_client: Key<RttClient>) -> Self {
        Self { session, rtt_client }
    }

    /// Async write to a down channel over the RPC session — no local `Core`
    /// (and no `block_on` bridge) required.
    pub(crate) async fn write_down_remote(
        &self,
        channel: u32,
        data: Vec<u8>,
    ) -> anyhow::Result<()> {
        self.session
            .send_to_rtt(self.rtt_client, channel, data)
            .await
    }
}


impl RttClientHandle {
    /// Poll multiple up channels in one go. The local path copies each
    /// channel's bytes (its `poll_channel` buffer can't span channels); the
    /// remote path `.await`s a single `rtt/poll_up` round trip. Per-channel
    /// errors are reported inline; a top-level `Err` means the batch failed.
    async fn poll_channels(
        &mut self,
        core: &mut Core<'_>,
        channels: &[u32],
    ) -> Result<ChannelPollResults, RttError> {
        match self {
            RttClientHandle::Local(client) => {
                let mut out = Vec::with_capacity(channels.len());
                for &channel in channels {
                    let res = client
                        .poll_channel(core, channel)
                        .map(|b| Cow::Owned(b.to_vec()));
                    out.push((channel, res));
                }
                Ok(out)
            }
            RttClientHandle::Remote(remote) => {
                let results = remote
                    .session
                    .poll_rtt_up(remote.rtt_client, channels.to_vec())
                    .await
                    .map_err(RttError::Other)?;
                Ok(results
                    .into_iter()
                    .map(|r| {
                        let res = match r.result {
                            Ok(data) => Ok(Cow::Owned(data)),
                            Err(e) => Err(RttError::Other(anyhow!(e))),
                        };
                        (r.channel, res)
                    })
                    .collect())
            }
        }
    }

    /// Remote-only poll: a single `rtt/poll_up` round trip with no local
    /// `Core` (and no `block_on` bridge). Panics for the `Local` variant —
    /// callers must branch on [`RttConnection::is_remote`].
    async fn poll_channels_remote(
        &mut self,
        channels: &[u32],
    ) -> Result<ChannelPollResults, RttError> {
        match self {
            RttClientHandle::Remote(remote) => {
                let results = remote
                    .session
                    .poll_rtt_up(remote.rtt_client, channels.to_vec())
                    .await
                    .map_err(RttError::Other)?;
                Ok(results
                    .into_iter()
                    .map(|r| {
                        let res = match r.result {
                            Ok(data) => Ok(Cow::Owned(data)),
                            Err(e) => Err(RttError::Other(anyhow!(e))),
                        };
                        (r.channel, res)
                    })
                    .collect())
            }
            RttClientHandle::Local(_) => Err(RttError::Other(anyhow!(
                "poll_channels_remote called on a local RTT client"
            ))),
        }
    }

    /// Restore the original mode of every up channel. The `Local` variant
    /// needs a live `Core`; the `Remote` variant `.await`s the
    /// `rtt/clean_up` round trip directly (no `block_on` bridge).
    async fn clean_up_async(&mut self, core: Option<&mut Core<'_>>) -> Result<(), RttError> {
        match self {
            RttClientHandle::Local(client) => {
                let core = core.ok_or_else(|| {
                    RttError::Other(anyhow!("local RTT clean_up needs a live Core"))
                })?;
                client.clean_up(core)
            }
            RttClientHandle::Remote(remote) => remote
                .session
                .clean_up_rtt(remote.rtt_client)
                .await
                .map_err(RttError::Other),
        }
    }

    /// Write data to a down channel.
    pub(crate) async fn write_down_async<B: crate::cmd::dap_server::backend::DapBackend>(
        &mut self,
        backend: &mut B,
        core_index: usize,
        channel: u32,
        data: Vec<u8>,
    ) -> Result<(), RttError> {
        match self {
            RttClientHandle::Local(client) => {
                let mut core = backend
                    .core(core_index)
                    .map_err(|e| RttError::Other(anyhow!(e)))?;
                client.write_down_channel(&mut core, channel, &data)
            }
            RttClientHandle::Remote(remote) => {
                remote
                    .write_down_remote(channel, data)
                    .await
                    .map_err(RttError::Other)
            }
        }
    }
}

/// Manage the active RTT target for a specific SessionData, as well as provide methods to reliably move RTT from target, through the debug_adapter, to the client.
pub struct RttConnection {
    /// The connection to RTT on the target
    pub(crate) client: RttClientHandle,
    /// Some status fields and methods to ensure continuity in flow of data from target to debugger to client.
    pub(crate) debugger_rtt_channels: Vec<DebuggerRttChannel>,
}

impl RttConnection {
    /// Whether this connection drives a server-side (RPC) RTT client.
    pub fn is_remote(&self) -> bool {
        matches!(self.client, RttClientHandle::Remote(_))
    }

    /// Polls all the available channels for data and transmits data to the client.
    /// If at least one channel had data, then return a `true` status.
    pub async fn process_rtt_data<P: ProtocolAdapter>(
        &mut self,
        debug_adapter: &mut DebugAdapter<P>,
        target_core: &mut Core<'_>,
    ) -> bool {
        // Only poll channels with an open client window; draining a closed
        // channel would drop target buffers prematurely.
        let windowed: Vec<u32> = self
            .debugger_rtt_channels
            .iter()
            .filter(|c| c.has_client_window)
            .map(|c| c.channel_number)
            .collect();
        if windowed.is_empty() {
            return false;
        }

        let results = match self.client.poll_channels(target_core, &windowed).await {
            Ok(results) => results,
            Err(error) => {
                debug_adapter
                    .show_error_message(&DebuggerError::Other(anyhow!(error)))
                    .ok();
                return false;
            }
        };

        self.emit_rtt_results(debug_adapter, results).await
    }

    /// Remote-only [`Self::process_rtt_data`]: a single `rtt/poll_up` round trip
    /// with no local `Core` (and no `block_on` bridge).
    pub async fn process_rtt_data_remote<P: ProtocolAdapter>(
        &mut self,
        debug_adapter: &mut DebugAdapter<P>,
    ) -> bool {
        let windowed: Vec<u32> = self
            .debugger_rtt_channels
            .iter()
            .filter(|c| c.has_client_window)
            .map(|c| c.channel_number)
            .collect();
        if windowed.is_empty() {
            return false;
        }

        let results = match self.client.poll_channels_remote(&windowed).await {
            Ok(results) => results,
            Err(error) => {
                debug_adapter
                    .show_error_message(&DebuggerError::Other(anyhow!(error)))
                    .ok();
                return false;
            }
        };

        self.emit_rtt_results(debug_adapter, results).await
    }

    async fn emit_rtt_results<P: ProtocolAdapter>(
        &mut self,
        debug_adapter: &mut DebugAdapter<P>,
        results: ChannelPollResults,
    ) -> bool {
        let mut at_least_one_channel_had_data = false;
        for (channel, result) in results {
            let Some(debugger_rtt_channel) = self
                .debugger_rtt_channels
                .iter_mut()
                .find(|c| c.channel_number == channel)
            else {
                continue;
            };

            let bytes = match result {
                Ok(bytes) => bytes,
                Err(error) => {
                    debug_adapter
                        .show_error_message(&DebuggerError::Other(anyhow!(error)))
                        .ok();
                    continue;
                }
            };

            at_least_one_channel_had_data |=
                debugger_rtt_channel.process_bytes(debug_adapter, &bytes);
        }
        at_least_one_channel_had_data
    }

    /// Clean up the RTT connection, restoring the state changes that we made.
    /// The `Local` variant needs a live `Core`; the `Remote` variant cleans up
    /// server-side (no `block_on` bridge).
    pub async fn clean_up_async(
        &mut self,
        target_core: Option<&mut Core<'_>>,
    ) -> Result<(), DebuggerError> {
        self.client
            .clean_up_async(target_core)
            .await
            .map_err(|err| DebuggerError::Other(anyhow!(err)))?;
        Ok(())
    }
}

pub(crate) struct DebuggerRttChannel {
    pub(crate) channel_number: u32,
    // We will not poll target RTT channels until we have confirmation from the client that the output window has been opened.
    pub(crate) has_client_window: bool,
    pub(crate) channel_data_format: RttDecoder,
}

impl DebuggerRttChannel {
    /// Decode already-fetched `bytes` for this channel and forward them.
    /// Returns whether any data was emitted.
    pub(crate) fn process_bytes<P: ProtocolAdapter>(
        &mut self,
        debug_adapter: &mut DebugAdapter<P>,
        bytes: &[u8],
    ) -> bool {
        match self.channel_data_format.process(bytes).ok().flatten() {
            Some(data) => debug_adapter.rtt_output(self.channel_number, data.to_string()),
            _ => false,
        }
    }
}
