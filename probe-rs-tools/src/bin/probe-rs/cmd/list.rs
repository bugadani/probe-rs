use crate::cmd::remote::ClientInterface;

#[derive(clap::Parser)]
pub struct Cmd {}

impl Cmd {
    pub async fn run(self, mut iface: impl ClientInterface) -> anyhow::Result<()> {
        let probes = iface.list_probes().await?;

        if !probes.is_empty() {
            println!("The following debug probes were found:");
            for (num, link) in probes.iter().enumerate() {
                println!("[{num}]: {link}");
            }
        } else {
            println!("No debug probes were found.");
        }
        Ok(())
    }
}
