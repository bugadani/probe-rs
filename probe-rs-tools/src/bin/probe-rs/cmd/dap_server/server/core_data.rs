use std::any::Any;
use std::ops::Range;

use super::session_data;
use crate::cmd::dap_server::backend::RttRemoteSeed;
use crate::cmd::dap_server::debug_adapter::dap::repl_commands::ReplCommand;
use crate::rpc::Key;
use crate::rpc::functions::rtt_client::ScanRegion as WireScanRegion;
use crate::util::rtt::client::RttClient;

/// `(channel number, channel name)` pairs returned while attaching to RTT.
pub(crate) type ChannelNames = Vec<(u32, String)>;
use crate::cmd::dap_server::{peripherals::svd_variables::SvdCache, server::debug_rtt};
use probe_rs::{Core, CoreStatus, rtt::ScanRegion};
use probe_rs_debug::{
    ObjectRef, VariableCache, debug_info::DebugInfo, stack_frame::StackFrameInfo,
};

/// Convert [`ScanRegion`] (probe-rs) into the wire [`WireScanRegion`].
pub(crate) fn wire_scan_region(scan: &ScanRegion) -> WireScanRegion {
    match scan {
        ScanRegion::Ram => WireScanRegion::Ram,
        ScanRegion::Ranges(ranges) => {
            WireScanRegion::Ranges(ranges.iter().map(|r| (r.start, r.end)).collect())
        }
        ScanRegion::Exact(addr) => WireScanRegion::Exact(*addr),
    }
}

/// [CoreData] is used to cache data needed by the debugger, on a per-core basis.
pub struct CoreData {
    pub core_index: usize,
    /// Track the last_known_status of the core.
    /// The debug client needs to be notified when the core changes state, and this can happen in one of two ways:
    /// 1. By polling the core status periodically (in [`crate::cmd::dap_server::server::debugger::Debugger::process_next_request()`]).
    ///    For instance, when the client sets the core running, and the core halts because of a breakpoint, we need to notify the client.
    /// 2. Some requests, like [`DebugAdapter::next()`], has an implicit action of setting the core running, before it waits for it to halt at the next statement.
    ///    To ensure the [`CoreHandle::poll_core()`] behaves correctly, it will set the `last_known_status` to [`CoreStatus::Running`],
    ///    and execute the request normally, with the expectation that the core will be halted, and that 1. above will detect this new status.
    ///    These 'implicit' updates of `last_known_status` will not(and should not) result in a notification to the client.
    pub last_known_status: CoreStatus,
    pub target_name: String,
    pub debug_info: Option<DebugInfo>,
    pub static_variables: Option<VariableCache>,
    pub core_peripherals: Option<SvdCache>,
    pub stack_frames: Vec<probe_rs_debug::stack_frame::StackFrame>,
    pub breakpoints: Vec<session_data::ActiveBreakpoint>,
    pub rtt_scan_ranges: ScanRegion,
    pub rtt_connection: Option<debug_rtt::RttConnection>,
    pub rtt_client: Option<RttClient>,
    /// When `Some`, RTT is driven through the server-side `RttClient` over
    /// RPC (RPC backend). When `None`, a local `RttClient` is used.
    pub rtt_remote_seed: Option<RttRemoteSeed>,
    /// Cache of the server-side RTT client handle between attach attempts,
    /// so we only call `create_rtt` once per core (RPC backend).
    pub rtt_remote_handle: Option<Key<RttClient>>,
    pub repl_commands: Vec<ReplCommand>,
    pub test_data: Box<dyn Any>,
}

/// [CoreHandle] provides handles to various data structures required to debug a single instance of a core. The actual state is stored in [session_data::SessionData].
pub struct CoreHandle<'p> {
    pub(crate) core: Core<'p>,
    pub(crate) core_data: &'p mut CoreData,
}

impl CoreHandle<'_> {
    /// Search available [`probe_rs::debug::StackFrame`]'s for the given `id`
    pub(crate) fn get_stackframe(
        &self,
        id: ObjectRef,
    ) -> Option<&probe_rs_debug::stack_frame::StackFrame> {
        self.core_data
            .stack_frames
            .iter()
            .find(|stack_frame| stack_frame.id == id)
    }

    /// Traverse all the variables in the available stack frames, and return the memory ranges
    /// required to resolve the values of these variables. This is used to provide the minimal
    /// memory ranges required to create a [`CoreDump`](probe_rs::CoreDump) for the current scope.
    pub(crate) fn get_memory_ranges(&mut self) -> Vec<Range<u64>> {
        let recursion_limit = 10;

        let mut all_discrete_memory_ranges = Vec::new();

        if let Some(static_variables) = &mut self.core_data.static_variables
            && let Some(debug_info) = self.core_data.debug_info.as_ref()
        {
            static_variables.recurse_deferred_variables(
                debug_info,
                &mut self.core,
                recursion_limit,
                StackFrameInfo {
                    registers: &self.core_data.stack_frames[0].registers,
                    frame_base: self.core_data.stack_frames[0].frame_base,
                    canonical_frame_address: self.core_data.stack_frames[0].canonical_frame_address,
                },
            );
            all_discrete_memory_ranges.append(&mut static_variables.get_discrete_memory_ranges());
        }

        // Expand and validate the static and local variables for each stack frame.
        for frame in self.core_data.stack_frames.iter_mut() {
            let mut variable_caches = Vec::new();
            if let Some(local_variables) = &mut frame.local_variables {
                variable_caches.push(local_variables);
            }
            for variable_cache in variable_caches {
                if let Some(debug_info) = self.core_data.debug_info.as_ref() {
                    // Cache the deferred top level children of the of the cache.
                    variable_cache.recurse_deferred_variables(
                        debug_info,
                        &mut self.core,
                        10,
                        StackFrameInfo {
                            registers: &frame.registers,
                            frame_base: frame.frame_base,
                            canonical_frame_address: frame.canonical_frame_address,
                        },
                    );
                    all_discrete_memory_ranges
                        .append(&mut variable_cache.get_discrete_memory_ranges());
                }
            }
            // Also capture memory addresses for essential registers.
            for register in frame.registers.0.iter() {
                if let Ok(Some(memory_range)) = register.memory_range() {
                    all_discrete_memory_ranges.push(memory_range);
                }
            }
        }
        // Consolidating all memory ranges that are withing 0x400 bytes of each other.
        consolidate_memory_ranges(all_discrete_memory_ranges, 0x400)
    }
}

/// Return a Vec of memory ranges that consolidate the adjacent memory ranges of the input ranges.
/// Note: The concept of "adjacent" is calculated to include a gap of up to specified number of bytes between ranges.
/// This serves to consolidate memory ranges that are separated by a small gap, but are still close enough for the purpose of the caller.
fn consolidate_memory_ranges(
    mut discrete_memory_ranges: Vec<Range<u64>>,
    include_bytes_between_ranges: u64,
) -> Vec<Range<u64>> {
    discrete_memory_ranges.sort_by_cached_key(|range| (range.start, range.end));
    discrete_memory_ranges.dedup();
    let mut consolidated_memory_ranges: Vec<Range<u64>> = Vec::new();
    let mut condensed_range: Option<Range<u64>> = None;

    for memory_range in discrete_memory_ranges.iter() {
        if let Some(range_comparator) = condensed_range {
            if memory_range.start <= range_comparator.end + include_bytes_between_ranges + 1 {
                let new_end = std::cmp::max(range_comparator.end, memory_range.end);
                condensed_range = Some(Range {
                    start: range_comparator.start,
                    end: new_end,
                });
            } else {
                consolidated_memory_ranges.push(range_comparator);
                condensed_range = Some(memory_range.clone());
            }
        } else {
            condensed_range = Some(memory_range.clone());
        }
    }

    if let Some(range_comparator) = condensed_range {
        consolidated_memory_ranges.push(range_comparator);
    }

    consolidated_memory_ranges
}

/// A single range should remain the same after consolidation.
#[test]
fn test_single_range() {
    let input = vec![Range { start: 0, end: 5 }];
    let expected = vec![Range { start: 0, end: 5 }];
    let result = consolidate_memory_ranges(input, 0);
    assert_eq!(result, expected);
}

/// Three ranges that are adjacent should be consolidated into one.
#[test]
fn test_three_adjacent_ranges() {
    let input = vec![
        Range { start: 0, end: 5 },
        Range { start: 6, end: 10 },
        Range { start: 11, end: 15 },
    ];
    let expected = vec![Range { start: 0, end: 15 }];
    let result = consolidate_memory_ranges(input, 0);
    assert_eq!(result, expected);
}

/// Two ranges that are distinct should remain distinct after consolidation.
#[test]
fn test_distinct_ranges() {
    let input = vec![Range { start: 0, end: 5 }, Range { start: 7, end: 10 }];
    let expected = vec![Range { start: 0, end: 5 }, Range { start: 7, end: 10 }];
    let result = consolidate_memory_ranges(input, 0);
    assert_eq!(result, expected);
}

/// Two ranges that are contiguous should be consolidated into one.
#[test]
fn test_contiguous_ranges() {
    let input = vec![Range { start: 0, end: 5 }, Range { start: 5, end: 10 }];
    let expected = vec![Range { start: 0, end: 10 }];
    let result = consolidate_memory_ranges(input, 0);
    assert_eq!(result, expected);
}

/// Three ranges where the first two are adjacent and the third is distinct should be consolidated into two.
#[test]
fn test_adjacent_and_distinct_ranges() {
    let input = vec![
        Range { start: 0, end: 5 },
        Range { start: 6, end: 10 },
        Range { start: 12, end: 15 },
    ];
    let expected = vec![Range { start: 0, end: 10 }, Range { start: 12, end: 15 }];
    let result = consolidate_memory_ranges(input, 0);
    assert_eq!(result, expected);
}

/// Two ranges where the second starts and ends before the first should remain distinct after consolidation.
#[test]
fn test_non_overlapping_ranges() {
    let input = vec![Range { start: 10, end: 20 }, Range { start: 0, end: 5 }];
    let expected = vec![Range { start: 0, end: 5 }, Range { start: 10, end: 20 }];
    let result = consolidate_memory_ranges(input, 0);
    assert_eq!(result, expected);
}

/// Two ranges where the second starts and ends before the first but are consolidated because they are within 5 bytes of each other.
#[test]
fn test_non_overlapping_ranges_with_extra_bytes() {
    let input = vec![Range { start: 10, end: 20 }, Range { start: 0, end: 5 }];
    let expected = vec![Range { start: 0, end: 20 }];
    let result = consolidate_memory_ranges(input, 5);
    assert_eq!(result, expected);
}

/// Two ranges where the second starts before, but intersects with the first, should be consolidated.
#[test]
fn test_reversed_intersecting_ranges() {
    let input = vec![Range { start: 10, end: 20 }, Range { start: 5, end: 15 }];
    let expected = vec![Range { start: 5, end: 20 }];
    let result = consolidate_memory_ranges(input, 0);
    assert_eq!(result, expected);
}
