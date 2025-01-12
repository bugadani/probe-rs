//! CLI-specific building blocks.

use std::{path::Path, sync::Arc, time::Instant};

use colored::Colorize;
use libtest_mimic::{Failed, Trial};
use probe_rs::{
    flashing::{BootInfo, FlashLayout, ProgressEvent},
    Session,
};
use tokio::{runtime::Handle, sync::Mutex};

use crate::{
    cmd::remote::{
        functions::{
            attach::AttachResult,
            flash::{DownloadOptions, FlashResult},
            monitor::{MonitorEvent, MonitorMode, MonitorOptions, SemihostingOutput},
            stack_trace::StackTrace,
            test::TestResult,
        },
        ClientInterface, Key, SessionInterface,
    },
    util::{
        common_options::{BinaryDownloadOptions, ProbeOptions},
        flash::CliProgressBars,
        logging,
    },
    FormatOptions,
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

pub async fn flash(
    session: &mut SessionInterface<'_, impl ClientInterface>,
    path: &Path,
    chip_erase: bool,
    format: FormatOptions,
    download_options: BinaryDownloadOptions,
) -> anyhow::Result<FlashResult> {
    // Start timer.
    let flash_timer = Instant::now();

    let flash_layout_output_path = download_options.flash_layout_output_path.clone();
    let pb = if download_options.disable_progressbars {
        None
    } else {
        Some(CliProgressBars::new())
    };

    let options = DownloadOptions {
        keep_unwritten_bytes: download_options.restore_unwritten,
        do_chip_erase: chip_erase,
        skip_erase: false,
        preverify: download_options.preverify,
        verify: download_options.verify,
        disable_double_buffering: download_options.disable_double_buffering,
    };

    let result = session
        .flash(path.to_path_buf(), format, options, move |event| {
            if let Some(ref path) = flash_layout_output_path {
                if let ProgressEvent::Initialized { ref phases, .. } = event {
                    let mut flash_layout = FlashLayout::default();
                    for phase_layout in phases {
                        flash_layout.merge_from(phase_layout.clone());
                    }

                    // Visualise flash layout to file if requested.
                    let visualizer = flash_layout.visualize();
                    _ = visualizer.write_svg(path);
                }
            }

            if let Some(ref pb) = pb {
                pb.handle(event);
            }
        })
        .await?;

    logging::eprintln(format!(
        "     {} in {:.02}s",
        "Finished".green().bold(),
        flash_timer.elapsed().as_secs_f32(),
    ));

    Ok(result)
}

pub async fn monitor(
    session: &mut SessionInterface<'_, impl ClientInterface>,
    mode: MonitorMode,
    path: &Path,
    options: MonitorOptions,
    print_stack_trace: bool,
) -> anyhow::Result<()> {
    let monitor = session.monitor(
        mode,
        path.to_path_buf(),
        options,
        move |event| match event {
            MonitorEvent::RttOutput(str) => print!("{}", str),
            MonitorEvent::SemihostingOutput(SemihostingOutput::StdOut(str)) => {
                print!("{}", str)
            }
            MonitorEvent::SemihostingOutput(SemihostingOutput::StdErr(str)) => {
                eprint!("{}", str)
            }
        },
    );

    tokio::select! {
        _ = monitor => Ok(()),
        _ = tokio::signal::ctrl_c() => {
            eprintln!("Received Ctrl+C, exiting");

            if print_stack_trace {
                display_stack_trace(session, path).await?;
            }

            Ok(())
        }
    }
}

pub async fn test(
    ctx: impl ClientInterface,
    sessid: Key<Session>,
    boot_info: BootInfo,
    libtest_args: libtest_mimic::Arguments,
    print_stack_trace: bool,
    path: &Path,
) -> anyhow::Result<()> {
    tracing::info!("libtest args {:?}", libtest_args);

    let shared_context = Arc::new(Mutex::new(ctx));
    let test = async {
        let tests = {
            let mut ctx = shared_context.lock().await;
            let mut session = SessionInterface::new(&mut *ctx, sessid);
            session.list_tests(boot_info).await?
        };

        let tests = tests
            .tests
            .into_iter()
            .map(|t| {
                let shared_context = shared_context.clone();
                let name = t.name.clone();
                let ignored = t.ignored;

                Trial::test(name, move || {
                    let mut ctx = shared_context.blocking_lock();
                    tokio::task::block_in_place(move || {
                        let mut session = SessionInterface::new(&mut *ctx, sessid);
                        let outcome =
                            Handle::current().block_on(async { session.run_test(t).await });

                        match outcome {
                            Ok(TestResult::Success) => Ok(()),
                            Ok(TestResult::Cancelled) => {
                                eprintln!("Cancelled");
                                std::process::exit(1);
                            }
                            Ok(TestResult::Failed(message, stack_trace)) => {
                                if let Some(stack_trace) = stack_trace {
                                    eprintln!("Stack trace:\n{}", stack_trace);
                                }
                                Err(Failed::from(message))
                            }
                            Err(e) => {
                                eprintln!("Error: {:?}", e);
                                std::process::exit(1);
                            }
                        }
                    })
                })
                .with_ignored_flag(ignored)
            })
            .collect::<Vec<_>>();

        tokio::task::spawn_blocking(move || {
            if libtest_mimic::run(&libtest_args, tests).has_failed() {
                anyhow::bail!("Some tests failed");
            }

            Ok(())
        })
        .await?
    };

    tokio::select! {
        result = test => result,
        _ = tokio::signal::ctrl_c() => {
            eprintln!("Received Ctrl+C, exiting");

            if print_stack_trace {
                let mut ctx = shared_context.lock().await;
                let mut session = SessionInterface::new(&mut *ctx, sessid);
                display_stack_trace(&mut session, path).await?;
            }

            Ok(())
        }
    }
}

async fn display_stack_trace(
    session: &mut SessionInterface<'_, impl ClientInterface>,
    path: &Path,
) -> anyhow::Result<()> {
    let stack_trace = session.stack_trace(path.to_path_buf()).await?;

    for StackTrace { core, frames } in stack_trace.cores.iter() {
        println!("Core {}", core);
        for frame in frames {
            println!("    {}", frame);
        }
    }

    Ok(())
}
