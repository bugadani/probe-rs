//! CLI-specific building blocks.

use crate::{
    cmd::remote::{functions::attach::AttachResult, ClientInterface, SessionInterface},
    util::common_options::ProbeOptions,
};

pub async fn attach_probe(
    iface: &mut impl ClientInterface,
    mut probe_options: ProbeOptions,
    resume_target: bool,
) -> anyhow::Result<SessionInterface<'_, impl ClientInterface>> {
    use anyhow::Context as _;
    use std::io::Write as _;

    // Load the chip description if provided.
    if let Some(chip_description) = probe_options.chip_description_path.take() {
        let file = std::fs::File::open(chip_description)?;

        // Load the YAML locally to validate it before sending it to the remote.
        let family_name = probe_rs::config::add_target_from_yaml(file)?;
        let family = probe_rs::config::get_family_by_name(&family_name)?;

        iface.load_chip_families(vec![family]).await?;
    }

    let result = iface
        .attach_probe(probe_options.clone(), resume_target)
        .await?;

    match result {
        AttachResult::Success(sessid) => Ok(SessionInterface::new(iface, sessid)),
        AttachResult::MultipleProbes(list) => {
            println!("Available Probes:");
            for (i, probe_info) in list.iter().enumerate() {
                println!("{i}: {probe_info}");
            }

            print!("Selection: ");
            std::io::stdout().flush().unwrap();

            let mut input = String::new();
            std::io::stdin()
                .read_line(&mut input)
                .expect("Expect input for probe selection");

            let probe_idx = input
                .trim()
                .parse::<usize>()
                .context("Failed to parse probe index")?;

            let probe = list
                .get(probe_idx)
                .ok_or_else(|| anyhow::anyhow!("Probe not found"))?;

            let mut probe_options = probe_options.clone();
            probe_options.probe = Some(probe.selector());

            match iface
                .attach_probe(probe_options.clone(), resume_target)
                .await?
            {
                AttachResult::Success(sessid) => Ok(SessionInterface::new(iface, sessid)),
                AttachResult::MultipleProbes(_) => {
                    anyhow::bail!("Did not expect multiple probes")
                }
            }
        }
    }
}
