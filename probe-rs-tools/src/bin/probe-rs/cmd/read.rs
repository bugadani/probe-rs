use crate::cmd::remote::ClientInterface;
use crate::util::cli;
use crate::util::common_options::{ProbeOptions, ReadWriteBitWidth, ReadWriteOptions};
use crate::CoreOptions;

/// Read from target memory address
///
/// e.g. probe-rs read b32 0x400E1490 2
///      Reads 2 32-bit words from address 0x400E1490
///
/// Output is a space separated list of hex values padded to the read word width.
/// e.g. 2 words
///     00 00 (8-bit)
///     00000000 00000000 (32-bit)
///     0000000000000000 0000000000000000 (64-bit)
///
/// NOTE: Only supports RAM addresses
#[derive(clap::Parser)]
#[clap(verbatim_doc_comment)]
pub struct Cmd {
    #[clap(flatten)]
    shared: CoreOptions,

    #[clap(flatten)]
    probe_options: ProbeOptions,

    #[clap(flatten)]
    read_write_options: ReadWriteOptions,

    /// Number of words to read from the target
    words: usize,
}

impl Cmd {
    pub async fn run(self, mut iface: impl ClientInterface) -> anyhow::Result<()> {
        let mut session = cli::attach_probe(&mut iface, self.probe_options, false).await?;
        let mut core = session.core(self.shared.core);

        match self.read_write_options.width {
            ReadWriteBitWidth::B8 => {
                let values = core
                    .read_memory::<u8>(self.read_write_options.address, self.words)
                    .await?;
                for val in values {
                    print!("{:02x} ", val);
                }
            }
            ReadWriteBitWidth::B16 => {
                let values = core
                    .read_memory::<u16>(self.read_write_options.address, self.words)
                    .await?;
                for val in values {
                    print!("{:08x} ", val);
                }
            }
            ReadWriteBitWidth::B32 => {
                let values = core
                    .read_memory::<u32>(self.read_write_options.address, self.words)
                    .await?;
                for val in values {
                    print!("{:08x} ", val);
                }
            }
            ReadWriteBitWidth::B64 => {
                let values = core
                    .read_memory::<u64>(self.read_write_options.address, self.words)
                    .await?;
                for val in values {
                    print!("{:016x} ", val);
                }
            }
        }
        println!();

        session.resume_all_cores().await?;

        Ok(())
    }
}
