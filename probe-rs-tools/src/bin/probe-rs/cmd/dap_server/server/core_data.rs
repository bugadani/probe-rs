use std::any::Any;

#[cfg(test)]
use std::ops::Range;

use super::session_data;
use crate::cmd::dap_server::backend::RttRemoteSeed;
use crate::cmd::dap_server::debug_adapter::dap::repl_commands::ReplCommand;
use crate::rpc::Key;
use crate::rpc::functions::rtt_client::ScanRegion as WireScanRegion;
use crate::util::rtt::client::RttClient;

/// `(channel number, channel name)` pairs returned while attaching to RTT.
pub(crate) type ChannelNames = Vec<(u32, String)>;
use crate::cmd::dap_server::server::debug_rtt;
use probe_rs::{CoreStatus, rtt::ScanRegion};

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
    /// Track the last status observed through the RPC backend.
    ///
    /// Periodic RPC status queries detect asynchronous transitions such as a
    /// breakpoint halt and notify the DAP client. Requests that resume or step
    /// the target update this field as part of their adapter bookkeeping so
    /// the next query can identify the subsequent transition without emitting
    /// a duplicate event for the request's own state change.
    pub last_known_status: CoreStatus,
    pub target_name: String,
    /// Metadata-only display cache of the server-owned stack state.
    ///
    /// This must be invalidated before any operation that can change the
    /// target's registers or execution state, and replaced only after a
    /// complete server unwind succeeds.
    pub stack_frames: Vec<probe_rs_debug::stack_frame::StackFrame>,
    pub breakpoints: Vec<session_data::ActiveBreakpoint>,
    pub rtt_scan_ranges: ScanRegion,
    pub rtt_connection: Option<debug_rtt::RttConnection>,
    /// Seed used to create and drive the server-side `RttClient` over RPC.
    /// This is `None` until RPC-backed core initialization provides the seed.
    pub rtt_remote_seed: Option<RttRemoteSeed>,
    /// Cache of the server-side RTT client handle between attach attempts,
    /// so we only call `create_rtt` once per core (RPC backend).
    pub rtt_remote_handle: Option<Key<RttClient>>,
    pub repl_commands: Vec<ReplCommand>,
    pub test_data: Box<dyn Any>,
}

impl CoreData {
    pub(crate) fn invalidate_stack_frame_cache(&mut self) {
        self.stack_frames.clear();
    }

    pub(crate) fn replace_stack_frame_cache(
        &mut self,
        frames: Vec<probe_rs_debug::stack_frame::StackFrame>,
    ) {
        self.stack_frames = frames;
    }
}

/// Return a Vec of memory ranges that consolidate the adjacent memory ranges of the input ranges.
/// Note: The concept of "adjacent" is calculated to include a gap of up to specified number of bytes between ranges.
/// This serves to consolidate memory ranges that are separated by a small gap, but are still close enough for the purpose of the caller.
#[cfg(test)]
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

#[test]
fn stack_frame_display_cache_is_invalidated_before_replacement() {
    use probe_rs::RegisterValue;
    use probe_rs_debug::{ObjectRef, registers::DebugRegisters, stack_frame::StackFrame};

    fn frame(id: i64) -> StackFrame {
        StackFrame {
            id: ObjectRef::from(id),
            function_name: format!("frame-{id}"),
            source_location: None,
            registers: DebugRegisters::default(),
            pc: RegisterValue::U32(0),
            frame_base: None,
            is_inlined: false,
            local_variables: None,
            canonical_frame_address: None,
        }
    }

    let mut core_data = CoreData {
        core_index: 0,
        last_known_status: CoreStatus::Unknown,
        target_name: String::new(),
        stack_frames: vec![frame(1)],
        breakpoints: vec![],
        rtt_scan_ranges: ScanRegion::Ranges(vec![]),
        rtt_connection: None,
        rtt_remote_seed: None,
        rtt_remote_handle: None,
        repl_commands: vec![],
        test_data: Box::new(()),
    };

    core_data.invalidate_stack_frame_cache();
    assert!(core_data.stack_frames.is_empty());

    core_data.replace_stack_frame_cache(vec![frame(2)]);
    assert_eq!(core_data.stack_frames.len(), 1);
    assert_eq!(core_data.stack_frames[0].id, ObjectRef::from(2));
}
