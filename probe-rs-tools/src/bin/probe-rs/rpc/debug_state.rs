use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;

use probe_rs_debug::{DebugInfo, StackFrame, VariableCache};

/// Per-session server-owned debug state. The RPC server builds and owns the
/// `VariableCache` trees (locals + statics) and the cached [`DebugInfo`], so
/// that variable expansion and value reads happen next to the target instead
/// of one round trip per memory read.
pub struct ServerDebugState {
    pub debug_info: Arc<DebugInfo>,
    pub per_core: HashMap<usize, CoreDebugState>,
    /// Per-core semihosting file state, owned server-side so that semihosting
    /// file I/O (open/read/write) happens next to the target. Decoupled from
    /// [`Self::per_core`] so it survives stack-frame refreshes on each halt.
    pub semihosting: HashMap<usize, CoreSemihostingState>,
}

#[derive(Default)]
pub struct CoreDebugState {
    pub stack_frames: Vec<StackFrame>,
    pub static_variables: Option<VariableCache>,
}

/// Server-side per-core semihosting state, mirroring the client's
/// `CoreData::semihosting_handles`/`next_semihosting_handle` so semihosting
/// file I/O can run next to the target.
pub struct CoreSemihostingState {
    pub handles: HashMap<u32, SemihostingFile>,
    pub next_handle: u32,
}

/// File descriptor for files opened by the target via semihosting.
pub struct SemihostingFile {
    pub handle: NonZeroU32,
    pub path: String,
    pub mode: &'static str,
}

impl ServerDebugState {
    pub fn new(debug_info: DebugInfo) -> Self {
        Self {
            debug_info: Arc::new(debug_info),
            per_core: HashMap::new(),
            semihosting: HashMap::new(),
        }
    }

    pub fn store_core(
        &mut self,
        core_index: usize,
        stack_frames: Vec<StackFrame>,
        static_variables: Option<VariableCache>,
    ) {
        self.per_core.insert(
            core_index,
            CoreDebugState {
                stack_frames,
                static_variables,
            },
        );
    }

    pub fn clear_core(&mut self, core_index: usize) {
        self.per_core.remove(&core_index);
    }

    /// Get the per-core semihosting state, creating it (with handles starting
    /// at 1024 to avoid collision with RTT channel numbers) on first access.
    pub fn semihosting_state(&mut self, core_index: usize) -> &mut CoreSemihostingState {
        self.semihosting.entry(core_index).or_insert_with(|| CoreSemihostingState {
            handles: HashMap::new(),
            next_handle: 1024,
        })
    }
}
