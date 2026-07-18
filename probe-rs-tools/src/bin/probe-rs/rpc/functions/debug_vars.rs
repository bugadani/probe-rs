use crate::rpc::{Key, functions::RpcContext};
use postcard_rpc::header::VarHeader;
use postcard_schema::Schema;
use probe_rs::Session;
use probe_rs_debug::{ObjectRef, StackFrameInfo, Variable, VariableCache, VariableName};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Schema)]
pub struct ScopesRequest {
    pub sessid: Key<Session>,
    pub core: u32,
    pub frame_id: u32,
}

#[derive(Serialize, Deserialize, Schema, Clone)]
pub struct WireScope {
    pub name: String,
    pub presentation_hint: Option<String>,
    pub variables_reference: i64,
    pub named_variables: Option<i64>,
    pub indexed_variables: Option<i64>,
    pub expensive: bool,
    pub line: Option<i64>,
    pub column: Option<i64>,
    pub end_line: Option<i64>,
    pub end_column: Option<i64>,
}

pub type ScopesResponse = crate::rpc::functions::RpcResult<Vec<WireScope>>;

#[derive(Serialize, Deserialize, Schema)]
pub struct VariablesRequest {
    pub sessid: Key<Session>,
    pub core: u32,
    pub variables_reference: u32,
    pub filter: Option<String>,
}

#[derive(Serialize, Deserialize, Schema, Clone)]
pub struct WireVariable {
    pub name: String,
    pub evaluate_name: Option<String>,
    pub memory_reference: Option<String>,
    pub indexed_variables: Option<i64>,
    pub named_variables: Option<i64>,
    pub presentation_hint: Option<String>,
    pub type_: Option<String>,
    pub value: String,
    pub variables_reference: i64,
}

pub type VariablesResponse = crate::rpc::functions::RpcResult<Vec<WireVariable>>;

/// Replicates `request_helpers::get_variable_reference` for the server-side path.
fn variable_reference(parent: &Variable, cache: &VariableCache) -> (ObjectRef, i64, i64) {
    if !parent.is_valid() {
        return (ObjectRef::Invalid, 0, 0);
    }
    let mut named = 0;
    let mut indexed = 0;
    for child in cache.get_children(parent.variable_key()) {
        if child.is_indexed() {
            indexed += 1;
        } else {
            named += 1;
        }
    }
    if named > 0 || indexed > 0 {
        (parent.variable_key(), named, indexed)
    } else if parent.variable_node_type.is_deferred() && parent.to_string(cache) != "()" {
        (parent.variable_key(), 0, 0)
    } else {
        (ObjectRef::Invalid, 0, 0)
    }
}

pub async fn scopes(
    ctx: &mut RpcContext,
    _header: VarHeader,
    request: ScopesRequest,
) -> ScopesResponse {
    let mut guard = ctx.debug_states().lock().await;
    let Some(state) = guard.get_mut(&request.sessid) else {
        Err("No debug state for session")?
    };
    let Some(core_state) = state.per_core.get_mut(&(request.core as usize)) else {
        Err("No debug state for core")?
    };

    let frame_ref = ObjectRef::from(request.frame_id as i64);
    let mut scopes: Vec<WireScope> = Vec::new();

    if let Some(static_cache) = &core_state.static_variables {
        scopes.push(WireScope {
            name: "Static".to_string(),
            presentation_hint: Some("statics".to_string()),
            variables_reference: i64::from(static_cache.root_variable().variable_key()),
            named_variables: None,
            indexed_variables: None,
            expensive: true,
            line: None,
            column: None,
            end_line: None,
            end_column: None,
        });
    }

    if let Some(frame) = core_state.stack_frames.iter().find(|f| f.id == frame_ref) {
        // Registers scope: reuse the frame id as its variables_reference.
        scopes.push(WireScope {
            name: "Registers".to_string(),
            presentation_hint: Some("registers".to_string()),
            variables_reference: i64::from(frame.id),
            named_variables: None,
            indexed_variables: None,
            expensive: true,
            line: None,
            column: None,
            end_line: None,
            end_column: None,
        });

        if let Some(locals) = &frame.local_variables {
            let line = frame
                .source_location
                .as_ref()
                .and_then(|l| l.line.map(|l| l as i64));
            let column = frame.source_location.as_ref().and_then(|l| {
                l.column.map(|c| match c {
                    probe_rs_debug::ColumnType::LeftEdge => 0,
                    probe_rs_debug::ColumnType::Column(c) => c as i64,
                })
            });
            scopes.push(WireScope {
                name: "Variables".to_string(),
                presentation_hint: Some("locals".to_string()),
                variables_reference: i64::from(locals.root_variable().variable_key()),
                named_variables: None,
                indexed_variables: None,
                expensive: false,
                line,
                column,
                end_line: None,
                end_column: None,
            });
        }
    }

    Ok(scopes)
}

pub async fn variables(
    ctx: &mut RpcContext,
    _header: VarHeader,
    request: VariablesRequest,
) -> VariablesResponse {
    let debug_info = {
        let guard = ctx.debug_states().lock().await;
        guard.get(&request.sessid).map(|s| s.debug_info.clone())
    };
    let Some(debug_info) = debug_info else {
        Err("No debug state for session")?
    };

    let mut session = ctx.session(request.sessid).await;
    let mut core = session.core(request.core as usize)?;

    let mut guard = ctx.debug_states().lock().await;
    let Some(state) = guard.get_mut(&request.sessid) else {
        Err("No debug state for session")?
    };
    let Some(core_state) = state.per_core.get_mut(&(request.core as usize)) else {
        Err("No debug state for core")?
    };

    let variable_ref = ObjectRef::from(request.variables_reference as i64);

    let mut parent_variable: Option<Variable> = None;
    let mut variable_cache: Option<&mut VariableCache> = None;
    let mut frame_info: Option<StackFrameInfo<'_>> = None;
    let mut cloned_registers: Option<probe_rs_debug::DebugRegisters> = None;

    // Registers special case: the variables_reference IS a frame id.
    if let Some(frame) = core_state
        .stack_frames
        .iter()
        .find(|f| f.id == variable_ref)
    {
        let regs: Vec<WireVariable> = frame
            .registers
            .0
            .iter()
            .map(|register| WireVariable {
                name: register.get_register_name(),
                evaluate_name: Some(register.get_register_name()),
                memory_reference: None,
                indexed_variables: None,
                named_variables: None,
                presentation_hint: None,
                type_: Some(format!("{}", VariableName::RegistersRoot)),
                value: register.value.unwrap_or_default().to_string(),
                variables_reference: 0,
            })
            .collect();
        return Ok(regs);
    }

    if let Some(search_cache) = core_state.static_variables.as_mut()
        && let Some(search_variable) = search_cache.get_variable_by_key(variable_ref)
    {
        parent_variable = Some(search_variable);
        variable_cache = Some(search_cache);
        if let Some(top_frame) = core_state.stack_frames.first() {
            cloned_registers = Some(top_frame.registers.clone());
            frame_info = Some(StackFrameInfo {
                registers: cloned_registers.as_ref().unwrap(),
                frame_base: top_frame.frame_base,
                canonical_frame_address: top_frame.canonical_frame_address,
            });
        }
    }

    if parent_variable.is_none() {
        for frame in core_state.stack_frames.iter_mut() {
            if let Some(search_cache) = frame.local_variables.as_mut()
                && let Some(search_variable) = search_cache.get_variable_by_key(variable_ref)
            {
                parent_variable = Some(search_variable);
                variable_cache = Some(search_cache);
                frame_info = Some(StackFrameInfo {
                    registers: &frame.registers,
                    frame_base: frame.frame_base,
                    canonical_frame_address: frame.canonical_frame_address,
                });
                break;
            }
        }
    }

    let Some(variable_cache) = variable_cache else {
        Err(format!(
            "No variable information found for {}!",
            request.variables_reference
        ))?
    };

    if let Some(parent) = parent_variable.as_mut()
        && parent.variable_node_type.is_deferred()
        && !variable_cache.has_children(parent)
    {
        if let Some(frame_info) = frame_info {
            debug_info.cache_deferred_variables(variable_cache, &mut core, parent, frame_info)?;
        }
    }

    let dap_variables: Vec<WireVariable> = variable_cache
        .get_children(variable_ref)
        .filter(|variable| match &request.filter {
            Some(filter) => match filter.as_str() {
                "indexed" => variable.is_indexed(),
                "named" => !variable.is_indexed(),
                _ => false,
            },
            None => true,
        })
        .map(|variable| {
            let (variables_reference, named_cnt, indexed_cnt) =
                variable_reference(variable, variable_cache);
            WireVariable {
                name: variable.name.to_string(),
                evaluate_name: None,
                memory_reference: Some(variable.memory_location.to_string()),
                indexed_variables: Some(indexed_cnt),
                named_variables: Some(named_cnt),
                presentation_hint: None,
                type_: Some(variable.type_name()),
                value: variable.to_string(variable_cache),
                variables_reference: i64::from(variables_reference),
            }
        })
        .collect();

    Ok(dap_variables)
}
