use std::collections::HashMap;
use std::sync::Arc;

use probe_rs_debug::{DebugInfo, StackFrame, VariableCache};

use crate::rpc::Key;
use probe_rs::Session;

/// Per-session server-owned debug state. The RPC server builds and owns the
/// `VariableCache` trees (locals + statics) and the cached [`DebugInfo`], so
/// that variable expansion and value reads happen next to the target instead
/// of one round trip per memory read.
pub struct ServerDebugState {
    pub debug_info: Arc<DebugInfo>,
    pub per_core: HashMap<usize, CoreDebugState>,
}

#[derive(Default)]
pub struct CoreDebugState {
    pub stack_frames: Vec<StackFrame>,
    pub static_variables: Option<VariableCache>,
}

impl ServerDebugState {
    pub fn new(debug_info: DebugInfo) -> Self {
        Self {
            debug_info: Arc::new(debug_info),
            per_core: HashMap::new(),
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
}
