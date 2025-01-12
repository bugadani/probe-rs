use jep106::JEP106Code;
use probe_rs::Target;
use probe_rs_target::{Chip, Core, MemoryRegion};
use serde::{Deserialize, Serialize};

use crate::cmd::remote::functions::{Context, EmitterFn, RemoteFunctions};

#[derive(Serialize, Deserialize)]
pub(in crate::cmd::remote) struct ListFamilies {}

impl ListFamilies {
    pub fn new() -> Self {
        Self {}
    }
}

impl super::RemoteFunction for ListFamilies {
    type Message = super::NoMessage;
    type Result = Vec<ChipFamily>;

    async fn run(self, _ctx: Context<'_, impl EmitterFn>) -> anyhow::Result<Vec<ChipFamily>> {
        Ok(probe_rs::config::families()
            .into_iter()
            .map(|f| f.into())
            .collect())
    }
}

impl From<ListFamilies> for RemoteFunctions {
    fn from(func: ListFamilies) -> Self {
        RemoteFunctions::ListChipFamilies(func)
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ChipFamily {
    /// This is the name of the chip family in base form.
    /// E.g. `nRF52832`.
    pub name: String,
    /// The JEP106 code of the manufacturer.
    pub manufacturer: Option<JEP106Code>,
    /// This vector holds all the variants of the family.
    pub variants: Vec<Chip>,
}

impl From<probe_rs_target::ChipFamily> for ChipFamily {
    fn from(value: probe_rs_target::ChipFamily) -> Self {
        Self {
            name: value.name,
            manufacturer: value.manufacturer,
            variants: value.variants,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(in crate::cmd::remote) struct ChipInfo {
    name: String,
}

impl ChipInfo {
    pub fn new(name: String) -> Self {
        Self { name }
    }
}

impl super::RemoteFunction for ChipInfo {
    type Message = super::NoMessage;
    type Result = ChipData;

    async fn run(self, _ctx: Context<'_, impl EmitterFn>) -> anyhow::Result<ChipData> {
        Ok(probe_rs::config::get_target_by_name(self.name)?.into())
    }
}

impl From<ChipInfo> for RemoteFunctions {
    fn from(func: ChipInfo) -> Self {
        RemoteFunctions::ChipInfo(func)
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ChipData {
    pub cores: Vec<Core>,
    pub memory_map: Vec<MemoryRegion>,
}

impl From<Target> for ChipData {
    fn from(value: Target) -> Self {
        Self {
            cores: value.cores,
            memory_map: value.memory_map,
        }
    }
}

// Used to avoid uploading a temp file to the remote.
#[derive(Serialize, Deserialize)]
pub struct LoadChipFamilies {
    pub families: Vec<probe_rs_target::ChipFamily>,
}

impl super::RemoteFunction for LoadChipFamilies {
    type Message = super::NoMessage;
    type Result = ();

    async fn run(self, ctx: Context<'_, impl EmitterFn>) -> anyhow::Result<()> {
        anyhow::ensure!(
            ctx.is_local(),
            "Loading chip families is not supported in the remote interface yet."
        );

        // TODO: this can only be done safely if we have separate registries per connection.
        for family in self.families {
            probe_rs::config::add_target_family(family)?;
        }
        Ok(())
    }
}

impl From<LoadChipFamilies> for RemoteFunctions {
    fn from(func: LoadChipFamilies) -> Self {
        RemoteFunctions::LoadChipFamilies(func)
    }
}
