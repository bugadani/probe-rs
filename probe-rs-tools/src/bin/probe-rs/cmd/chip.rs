use std::io::Write;

use bytesize::ByteSize;
use probe_rs::config::MemoryRegion;

use crate::cmd::remote::ClientInterface;

#[derive(clap::Parser)]
pub struct Cmd {
    #[clap(subcommand)]
    subcommand: Subcommand,
}

#[derive(clap::Subcommand)]
/// Inspect internal registry of supported chips
enum Subcommand {
    /// Lists all the available families and their chips with their full.
    #[clap(name = "list")]
    List,
    /// Shows chip properties of a specific chip
    #[clap(name = "info")]
    Info {
        /// The name of the chip to display.
        name: String,
    },
}

impl Cmd {
    pub async fn run(self, mut iface: impl ClientInterface) -> anyhow::Result<()> {
        let output = std::io::stdout().lock();

        match self.subcommand {
            Subcommand::List => print_families(&mut iface, output).await,
            Subcommand::Info { name } => print_chip_info(&mut iface, output, &name).await,
        }
    }
}

/// Print all the available families and their contained chips to the
/// commandline.
pub async fn print_families(
    iface: &mut impl ClientInterface,
    mut output: impl Write,
) -> anyhow::Result<()> {
    writeln!(output, "Available chips:")?;
    let families = iface.list_chip_families().await?;
    for family in families {
        writeln!(output, "{}", &family.name)?;
        writeln!(output, "    Variants:")?;
        for variant in family.variants {
            writeln!(output, "        {}", variant.name)?;
        }
    }
    Ok(())
}

/// Print all the available families and their contained chips to the
/// commandline.
pub async fn print_chip_info(
    iface: &mut impl ClientInterface,
    mut output: impl Write,
    name: &str,
) -> anyhow::Result<()> {
    writeln!(output, "{}", name)?;
    let target = iface.chip_info(name).await?;
    writeln!(output, "Cores ({}):", target.cores.len())?;
    for core in target.cores {
        writeln!(
            output,
            "    - {} ({:?})",
            core.name.to_ascii_lowercase(),
            core.core_type
        )?;
    }

    fn get_range_len(range: &std::ops::Range<u64>) -> u64 {
        range.end - range.start
    }

    for memory in target.memory_map {
        let range = memory.address_range();
        let size = ByteSize(get_range_len(&range)).to_string_as(true);
        let kind = match memory {
            MemoryRegion::Ram(_) => "RAM",
            MemoryRegion::Generic(_) => "Generic",
            MemoryRegion::Nvm(_) => "NVM",
        };
        writeln!(output, "{kind}: {range:#010x?} ({size})")?
    }
    Ok(())
}

#[cfg(test)]
#[tokio::test]
async fn single_chip_output() {
    let mut buff = Vec::new();
    let mut iface = crate::cmd::remote::LocalSession::new();

    print_chip_info(&mut iface, &mut buff, "nrf52840_xxaa")
        .await
        .unwrap();

    // output should be valid utf8
    let output = String::from_utf8(buff).unwrap();

    insta::assert_snapshot!(output);
}

#[cfg(test)]
#[tokio::test]
async fn multiple_chip_output() {
    let mut buff = Vec::new();

    let mut iface = crate::cmd::remote::LocalSession::new();
    let error = print_chip_info(&mut iface, &mut buff, "nrf52")
        .await
        .unwrap_err();

    insta::assert_snapshot!(error.to_string());
}
