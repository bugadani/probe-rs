use std::borrow::Cow;

use crate::util::rtt::client::RttClient;
use crate::{
    cmd::dap_server::{
        DebuggerError,
        backend::block_on,
        debug_adapter::{dap::adapter::*, protocol::ProtocolAdapter},
    },
    rpc::{Key, client::SessionInterface},
    util::rtt::RttDecoder,
};
use anyhow::anyhow;
use probe_rs::{Core, rtt::Error as RttError};
use tokio::runtime::Handle;

/// Per-channel result of a batched [`RttClientHandle::poll_channels`] call:
/// newly-available bytes (or a per-channel error), keyed by channel number.
pub(crate) type ChannelPollResults<'c> = Vec<(u32, Result<Cow<'c, [u8]>, RttError>)>;

/// The RTT client the DAP server drives.
///
/// `Local` is the historical path: a [`RttClient`] that reads the target's
/// RTT control block and channel buffers directly through a [`Core`]. With
/// the RPC backend that `Core` is a `RpcRemoteCore`, so every poll expands to
/// many memory-read round trips.
///
/// `Remote` instead drives the **server-side** `RttClient` (created via the
/// `create_rtt` RPC endpoint and stored under a [`Key`]) so each poll is a
/// single `rtt/poll_up` round trip.
pub enum RttClientHandle {
    Local(RttClient),
    Remote(RemoteRttClient),
}

/// Handle to the server-side [`RttClient`], used by the RPC-backed DAP server.
pub struct RemoteRttClient {
    handle: Handle,
    session: SessionInterface,
    rtt_client: Key<RttClient>,
}

impl RemoteRttClient {
    pub(crate) fn new(
        handle: Handle,
        session: SessionInterface,
        rtt_client: Key<RttClient>,
    ) -> Self {
        Self {
            handle,
            session,
            rtt_client,
        }
    }
}

impl RttClientHandle {
    /// Poll multiple up channels in one go and return the newly-available
    /// bytes for each, keyed by channel number.
    ///
    /// The local path copies each channel's buffered bytes (the local
    /// `RttClient::poll_channel` borrows an internal buffer that cannot
    /// be held across multiple channels). The remote path issues a single
    /// `rtt/poll_up` round trip for all channels.
    ///
    /// Per-channel errors are reported inline (as `Err`) so the caller can
    /// surface them per channel; a top-level `Err` means the batch itself
    /// failed (e.g. the RPC request errored).
    fn poll_channels<'c>(
        &'c mut self,
        core: &mut Core<'_>,
        channels: &[u32],
    ) -> Result<ChannelPollResults<'c>, RttError> {
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
                let results = block_on(
                    &remote.handle,
                    remote
                        .session
                        .poll_rtt_up(remote.rtt_client, channels.to_vec()),
                )
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

    /// Restore the original mode of every up channel.
    fn clean_up(&mut self, core: &mut Core<'_>) -> Result<(), RttError> {
        match self {
            RttClientHandle::Local(client) => client.clean_up(core),
            RttClientHandle::Remote(remote) => block_on(
                &remote.handle,
                remote.session.clean_up_rtt(remote.rtt_client),
            )
            .map_err(RttError::Other),
        }
    }

    /// Write data to a down channel.
    pub(crate) fn write_down(
        &mut self,
        core: &mut Core<'_>,
        channel: u32,
        data: &[u8],
    ) -> Result<(), RttError> {
        match self {
            RttClientHandle::Local(client) => client.write_down_channel(core, channel, data),
            RttClientHandle::Remote(remote) => block_on(
                &remote.handle,
                remote
                    .session
                    .send_to_rtt(remote.rtt_client, channel, data.to_vec()),
            )
            .map_err(RttError::Other),
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
    /// Polls all the available channels for data and transmits data to the client.
    /// If at least one channel had data, then return a `true` status.
    pub fn process_rtt_data<P: ProtocolAdapter>(
        &mut self,
        debug_adapter: &mut DebugAdapter<P>,
        target_core: &mut Core<'_>,
    ) -> bool {
        // Only poll channels that the client has opened an output window
        // for — pulling from a channel with no window would drain target
        // buffers prematurely.
        let windowed: Vec<u32> = self
            .debugger_rtt_channels
            .iter()
            .filter(|c| c.has_client_window)
            .map(|c| c.channel_number)
            .collect();
        if windowed.is_empty() {
            return false;
        }

        let results = match self.client.poll_channels(target_core, &windowed) {
            Ok(results) => results,
            Err(error) => {
                debug_adapter
                    .show_error_message(&DebuggerError::Other(anyhow!(error)))
                    .ok();
                return false;
            }
        };

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
    pub fn clean_up(&mut self, target_core: &mut Core<'_>) -> Result<(), DebuggerError> {
        self.client
            .clean_up(target_core)
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
    /// Decode already-fetched `bytes` for this channel and forward them to
    /// the DAP client. Returns whether any data was emitted.
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
