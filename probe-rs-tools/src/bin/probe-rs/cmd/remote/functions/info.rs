//! Read information about the connected target using the selected wire protocol.
//!
//! The information is passed as a stream of messages to the provided emitter.

use anyhow::anyhow;
use probe_rs::{
    architecture::{
        arm::{
            ap::{ApClass, MemoryApType, IDR},
            armv6m::Demcr,
            component::Scs,
            dp::{Ctrl, DebugPortId, DebugPortVersion, DLPIDR, DPIDR, TARGETID},
            memory::{
                romtable::{PeripheralID, RomTable},
                Component, ComponentId, CoresightComponent, PeripheralType,
            },
            sequences::DefaultArmSequence,
            ArmProbeInterface, DpAddress, FullyQualifiedApAddress, Register,
        },
        riscv::communication_interface::RiscvCommunicationInterface,
        xtensa::communication_interface::{
            XtensaCommunicationInterface, XtensaDebugInterfaceState,
        },
    },
    probe::{Probe, WireProtocol},
    MemoryMappedRegister,
};
use serde::{Deserialize, Serialize};

use crate::{
    cmd::remote::functions::{Context, EmitterFn, RemoteFunction, RemoteFunctions},
    util::common_options::ProbeOptions,
};

#[derive(Serialize, Deserialize)]
pub(in crate::cmd::remote) struct Info {
    pub probe_options: ProbeOptions,
    pub target_sel: Option<u32>,
    pub protocol: WireProtocol,
}

impl RemoteFunction for Info {
    type Message = InfoEvent;
    type Result = ();

    async fn run(self, mut ctx: Context<'_, impl EmitterFn>) -> anyhow::Result<()> {
        let probe_options = self.probe_options.load()?;

        let probe = probe_options.attach_probe(&ctx.lister())?;

        let protocol = self.protocol;

        if let Err(e) = try_show_info(
            &mut ctx,
            probe,
            protocol,
            probe_options.connect_under_reset(),
            self.target_sel,
        )
        .await
        {
            ctx.emit(InfoEvent::Message(format!(
                "Failed to identify target using protocol {protocol}: {e:?}"
            )))
            .await?;
        }

        Ok(())
    }
}

impl From<Info> for RemoteFunctions {
    fn from(func: Info) -> Self {
        RemoteFunctions::Info(func)
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub enum InfoEvent {
    Message(String),
    ProtocolNotSupportedByArch {
        architecture: String,
        protocol: WireProtocol,
    },
    ProbeInterfaceMissing {
        interface: String,
        architecture: String,
    },
    Error {
        architecture: String,
        error: String,
    },
    ArmError {
        dp_addr: DpAddress,
        error: String,
    },
    Idcode {
        architecture: String,
        idcode: Option<u32>,
    },
    ArmDp(DebugPortInfo),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DebugPortInfoNode {
    pub dp_info: DebugPortId,
    pub targetid: u32,
    pub dlpidr: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DebugPortInfo {
    pub dp_info: DebugPortInfoNode,
    pub aps: Vec<ApInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ApInfo {
    MemoryAp {
        ap_addr: FullyQualifiedApAddress,
        component_tree: ComponentTreeNode,
    },
    Unknown {
        ap_addr: FullyQualifiedApAddress,
        idr: IDR,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComponentTreeNode {
    pub node: String,
    pub children: Vec<ComponentTreeNode>,
}

impl From<String> for ComponentTreeNode {
    fn from(node: String) -> Self {
        Self::new(node)
    }
}

impl ComponentTreeNode {
    fn new(node: String) -> Self {
        Self {
            node,
            children: vec![],
        }
    }

    fn push(&mut self, child: impl Into<ComponentTreeNode>) {
        self.children.push(child.into());
    }
}

async fn try_show_info(
    ctx: &mut Context<'_, impl EmitterFn>,
    mut probe: Probe,
    protocol: WireProtocol,
    connect_under_reset: bool,
    target_sel: Option<u32>,
) -> anyhow::Result<()> {
    probe.select_protocol(protocol)?;

    if connect_under_reset {
        probe.attach_to_unspecified_under_reset()?;
    } else {
        probe.attach_to_unspecified()?;
    }

    if probe.has_arm_interface() {
        let dp_addr = if let Some(target_sel) = target_sel {
            vec![DpAddress::Multidrop(target_sel)]
        } else {
            vec![
                DpAddress::Default,
                // RP2040
                DpAddress::Multidrop(0x01002927),
                DpAddress::Multidrop(0x11002927),
            ]
        };

        for address in dp_addr {
            match try_show_arm_dp_info(ctx, probe, address).await {
                (probe_moved, Ok(dp_version)) => {
                    probe = probe_moved;
                    if dp_version < DebugPortVersion::DPv2 && target_sel.is_none() {
                        let message = format!("Debug port version {dp_version} does not support SWD multidrop. Stopping here.");
                        ctx.emit(InfoEvent::Message(message)).await?;
                        break;
                    }
                }
                (probe_moved, Err(e)) => {
                    probe = probe_moved;
                    ctx.emit(InfoEvent::ArmError {
                        dp_addr: address,
                        error: format!("{e:?}"),
                    })
                    .await?;
                }
            }
        }
    } else {
        ctx.emit(InfoEvent::ProbeInterfaceMissing {
            interface: "DAP".to_string(),
            architecture: "ARM".to_string(),
        })
        .await?;
    }

    // This check is a bit weird, but `try_into_riscv_interface` will try to switch the protocol to JTAG.
    // If the current protocol we want to use is SWD, we have avoid this.
    if probe.has_riscv_interface() && protocol == WireProtocol::Jtag {
        tracing::debug!("Trying to show RISC-V chip information");
        match probe.try_get_riscv_interface_builder() {
            Ok(factory) => {
                let mut state = factory.create_state();
                match factory.attach(&mut state) {
                    Ok(mut interface) => {
                        if let Err(error) = show_riscv_info(ctx, &mut interface).await {
                            ctx.emit(InfoEvent::Error {
                                architecture: "RISC-V".to_string(),
                                error: format!("{error:?}"),
                            })
                            .await?;
                        }
                    }
                    Err(error) => {
                        ctx.emit(InfoEvent::Error {
                            architecture: "RISC-V".to_string(),
                            error: format!("{error:?}"),
                        })
                        .await?;
                    }
                };
            }
            Err(error) => {
                ctx.emit(InfoEvent::Error {
                    architecture: "RISC-V".to_string(),
                    error: format!("{error:?}"),
                })
                .await?;
            }
        }
    } else if protocol == WireProtocol::Swd {
        ctx.emit(InfoEvent::ProtocolNotSupportedByArch {
            architecture: "RISC-V".to_string(),
            protocol,
        })
        .await?;
    } else {
        ctx.emit(InfoEvent::ProbeInterfaceMissing {
            interface: "RISC-V".to_string(),
            architecture: "RISC-V".to_string(),
        })
        .await?;
    }

    // This check is a bit weird, but `try_into_xtensa_interface` will try to switch the protocol to JTAG.
    // If the current protocol we want to use is SWD, we have avoid this.
    if probe.has_xtensa_interface() && protocol == WireProtocol::Jtag {
        tracing::debug!("Trying to show Xtensa chip information");
        let mut state = XtensaDebugInterfaceState::default();
        match probe.try_get_xtensa_interface(&mut state) {
            Ok(mut interface) => {
                if let Err(error) = show_xtensa_info(ctx, &mut interface).await {
                    ctx.emit(InfoEvent::Error {
                        architecture: "Xtensa".to_string(),
                        error: format!("{error:?}"),
                    })
                    .await?;
                }
            }
            Err(error) => {
                ctx.emit(InfoEvent::Error {
                    architecture: "Xtensa".to_string(),
                    error: format!("{error:?}"),
                })
                .await?;
            }
        }
    } else if protocol == WireProtocol::Swd {
        ctx.emit(InfoEvent::ProtocolNotSupportedByArch {
            architecture: "Xtensa".to_string(),
            protocol,
        })
        .await?;
    } else {
        ctx.emit(InfoEvent::ProbeInterfaceMissing {
            interface: "Xtensa".to_string(),
            architecture: "Xtensa".to_string(),
        })
        .await?;
    }

    Ok(())
}

async fn try_show_arm_dp_info(
    ctx: &mut Context<'_, impl EmitterFn>,
    probe: Probe,
    dp_address: DpAddress,
) -> (Probe, anyhow::Result<DebugPortVersion>) {
    tracing::debug!("Trying to show ARM chip information");
    match probe
        .try_into_arm_interface()
        .map_err(|(iface, e)| (iface, anyhow!(e)))
        .and_then(|interface| {
            interface
                .initialize(DefaultArmSequence::create(), dp_address)
                .map_err(|(interface, e)| (interface.close(), anyhow!(e)))
        }) {
        Ok(mut interface) => {
            let res = show_arm_info(ctx, &mut *interface, dp_address).await;
            (interface.close(), res)
        }
        Err((probe, e)) => (probe, Err(e)),
    }
}

/// Try to show information about the ARM chip, connected to a DP at the given address.
///
/// Returns the version of the DP.
async fn show_arm_info(
    ctx: &mut Context<'_, impl EmitterFn>,
    interface: &mut dyn ArmProbeInterface,
    dp: DpAddress,
) -> anyhow::Result<DebugPortVersion> {
    let dp_info = interface.read_raw_dp_register(dp, DPIDR::ADDRESS)?;
    let dp_info = DebugPortId::from(DPIDR(dp_info));

    let dpinfo = if dp_info.version == DebugPortVersion::DPv2 {
        let targetid = interface.read_raw_dp_register(dp, TARGETID::ADDRESS)?;

        // Read Instance ID
        let dlpidr = interface.read_raw_dp_register(dp, DLPIDR::ADDRESS)?;

        // Read from the CTRL/STAT register, to ensure that the dpbanksel field is set to zero.
        // This helps with error handling later, because it means the CTRL/AP register can be
        // read in case of an error.
        let _ = interface.read_raw_dp_register(dp, Ctrl::ADDRESS)?;

        DebugPortInfoNode {
            dp_info,
            targetid,
            dlpidr,
        }
    } else {
        DebugPortInfoNode {
            dp_info,
            targetid: 0,
            dlpidr: 0,
        }
    };

    let mut info = DebugPortInfo {
        dp_info: dpinfo.clone(),
        aps: vec![],
    };

    let access_ports = interface.access_ports(dp)?;
    ctx.emit(InfoEvent::Message(format!(
        "ARM Chip with debug port {:x?}:",
        dp
    )))
    .await?;

    for ap_address in access_ports {
        use probe_rs::architecture::arm::ap::IDR;
        let idr: IDR = interface
            .read_raw_ap_register(&ap_address, IDR::ADDRESS)?
            .try_into()?;

        let ap_info = if idr.CLASS == ApClass::MemAp {
            let component_tree = match handle_memory_ap(interface, &ap_address) {
                Ok(component_tree) => component_tree,
                Err(e) => ComponentTreeNode::new(format!("Error during access: {e}")),
            };
            ApInfo::MemoryAp {
                ap_addr: ap_address,
                component_tree,
            }
        } else {
            ApInfo::Unknown {
                ap_addr: ap_address,
                idr,
            }
        };

        info.aps.push(ap_info);
    }

    ctx.emit(InfoEvent::ArmDp(info)).await?;

    Ok(dpinfo.dp_info.version)
}

fn handle_memory_ap(
    interface: &mut dyn ArmProbeInterface,
    access_port: &FullyQualifiedApAddress,
) -> anyhow::Result<ComponentTreeNode> {
    let component = {
        let mut memory = interface.memory_interface(access_port)?;

        // Check if the AP is accessible
        let (interface, ap) = memory.try_as_parts()?;
        let csw = ap.generic_status(interface)?;
        if !csw.DeviceEn {
            return Ok(ComponentTreeNode::new(
                "Memory AP is not accessible, DeviceEn bit not set".to_string(),
            ));
        }

        let base_address = memory.base_address()?;
        let mut demcr = Demcr(memory.read_word_32(Demcr::get_mmio_address())?);
        demcr.set_dwtena(true);
        memory.write_word_32(Demcr::get_mmio_address(), demcr.into())?;
        Component::try_parse(&mut *memory, base_address)?
    };
    let component_tree = coresight_component_tree(interface, component, access_port)?;

    Ok(component_tree)
}

fn coresight_component_tree(
    interface: &mut dyn ArmProbeInterface,
    component: Component,
    access_port: &FullyQualifiedApAddress,
) -> anyhow::Result<ComponentTreeNode> {
    let tree = match &component {
        Component::GenericVerificationComponent(_) => ComponentTreeNode::new("Generic".to_string()),
        Component::Class1RomTable(id, table) => {
            let peripheral_id = id.peripheral_id();

            let root = if let Some(part) = peripheral_id.determine_part() {
                format!("{} (ROM Table, Class 1)", part.name())
            } else {
                match peripheral_id.designer() {
                    Some(designer) => format!("ROM Table (Class 1), Designer: {designer}"),
                    None => "ROM Table (Class 1)".to_string(),
                }
            };

            let mut tree = ComponentTreeNode::new(root);
            process_vendor_rom_tables(interface, id, table, access_port, &mut tree)?;

            for entry in table.entries() {
                let component = entry.component().clone();

                tree.push(coresight_component_tree(interface, component, access_port)?);
            }

            tree
        }
        Component::CoresightComponent(id) => {
            let peripheral_id = id.peripheral_id();

            let component_description = if let Some(part_info) = peripheral_id.determine_part() {
                format!("{: <15} (Coresight Component)", part_info.name())
            } else {
                format!(
                    "Coresight Component, Part: {:#06x}, Devtype: {:#04x}, Archid: {:#06x}, Designer: {}",
                    peripheral_id.part(),
                    peripheral_id.dev_type(),
                    peripheral_id.arch_id(),
                    peripheral_id.designer()
                        .unwrap_or("<unknown>"),
                )
            };

            let mut tree = ComponentTreeNode::new(component_description);
            process_component_entry(&mut tree, interface, peripheral_id, &component, access_port)?;

            tree
        }

        Component::PeripheralTestBlock(_) => {
            ComponentTreeNode::new("Peripheral test block".to_string())
        }
        Component::GenericIPComponent(id) => {
            let peripheral_id = id.peripheral_id();

            let desc = if let Some(part_desc) = peripheral_id.determine_part() {
                format!("{: <15} (Generic IP component)", part_desc.name())
            } else {
                "Generic IP component".to_string()
            };

            let mut tree = ComponentTreeNode::new(desc);
            process_component_entry(&mut tree, interface, peripheral_id, &component, access_port)?;

            tree
        }

        Component::CoreLinkOrPrimeCellOrSystemComponent(_) => {
            ComponentTreeNode::new("Core Link / Prime Cell / System component".to_string())
        }
    };

    Ok(tree)
}

/// Processes information from/around manufacturer-specific ROM tables and adds them to the tree.
///
/// Some manufacturer-specific ROM tables contain more than just entries. This function tries
/// to make sense of these tables.
fn process_vendor_rom_tables(
    interface: &mut dyn ArmProbeInterface,
    id: &ComponentId,
    _table: &RomTable,
    access_port: &FullyQualifiedApAddress,
    tree: &mut ComponentTreeNode,
) -> anyhow::Result<()> {
    let peripheral_id = id.peripheral_id();
    let Some(part_info) = peripheral_id.determine_part() else {
        return Ok(());
    };

    if part_info.peripheral_type() == PeripheralType::Custom && part_info.name() == "Atmel DSU" {
        use probe_rs::vendor::microchip::sequences::atsam::DsuDid;

        // Read and parse the DID register
        let did = DsuDid(
            interface
                .memory_interface(access_port)?
                .read_word_32(DsuDid::ADDRESS)?,
        );

        tree.push(format!("Atmel device (DID = {:#010x})", did.0));
    }

    Ok(())
}

/// Processes ROM table entries and adds them to the tree.
fn process_component_entry(
    tree: &mut ComponentTreeNode,
    interface: &mut dyn ArmProbeInterface,
    peripheral_id: &PeripheralID,
    component: &Component,
    access_port: &FullyQualifiedApAddress,
) -> anyhow::Result<()> {
    let Some(part) = peripheral_id.determine_part() else {
        return Ok(());
    };

    if part.peripheral_type() == PeripheralType::Scs {
        let cc = &CoresightComponent::new(component.clone(), access_port.clone());
        let scs = &mut Scs::new(interface, cc);
        let cpu_tree = cpu_info_tree(scs)?;

        tree.push(cpu_tree);
    }

    Ok(())
}

fn cpu_info_tree(scs: &mut Scs) -> anyhow::Result<ComponentTreeNode> {
    let mut tree = ComponentTreeNode::new("CPUID".into());

    let cpuid = scs.cpuid()?;

    tree.push(format!("IMPLEMENTER: {}", cpuid.implementer_name()));
    tree.push(format!("VARIANT: {}", cpuid.variant()));
    tree.push(format!("PARTNO: {}", cpuid.part_name()));
    tree.push(format!("REVISION: {}", cpuid.revision()));

    Ok(tree)
}

async fn show_riscv_info(
    ctx: &mut Context<'_, impl EmitterFn>,
    interface: &mut RiscvCommunicationInterface<'_>,
) -> anyhow::Result<()> {
    let idcode = interface.read_idcode()?;

    ctx.emit(InfoEvent::Idcode {
        architecture: "RISC-V".to_string(),
        idcode,
    })
    .await
}

async fn show_xtensa_info(
    ctx: &mut Context<'_, impl EmitterFn>,
    interface: &mut XtensaCommunicationInterface<'_>,
) -> anyhow::Result<()> {
    let idcode = interface.read_idcode()?;

    ctx.emit(InfoEvent::Idcode {
        architecture: "Xtensa".to_string(),
        idcode: Some(idcode),
    })
    .await
}
