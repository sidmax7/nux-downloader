//! `nuxd`: thin CLI over `nuxd-core` (`ARCHITECTURE.md` §15). The primary
//! way to exercise the engine before the Tauri UI exists (Phase 6).

mod progress;

use clap::{Parser, Subcommand};
use nuxd_core::config::EngineConfig;
use nuxd_core::model::{PackageId, PackageStatus};
use nuxd_core::net::monitor::{self, LinkSetup, MonitorSetup};
use nuxd_core::{AddOptions, Engine};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "nuxd",
    version,
    about = "Nux Downloader — multi-link, multi-file download manager"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Where `queue.json` and package manifests live (default: the
    /// platform data directory via `directories`). Mainly for tests and
    /// running more than one independent queue side by side.
    #[arg(long, global = true, value_name = "DIR")]
    data_dir: Option<PathBuf>,
    /// Bytes buffered before each positional write and checkpoint-cadence
    /// check (default: 256 KiB, `ARCHITECTURE.md` §19). Lowering it
    /// trades some write efficiency for more frequent, finer-grained
    /// checkpoints — useful on a flaky link where you'd rather resume
    /// close to where a crash happened than replay a large buffer's
    /// worth of already-received bytes.
    #[arg(long, global = true, value_name = "BYTES")]
    write_buffer_bytes: Option<usize>,
    /// Don't discover or bind to network interfaces: use one link and let
    /// the OS route every connection.
    #[arg(long, global = true)]
    unbound: bool,
}

/// The global options that shape how the engine starts.
struct StartOpts {
    data_dir: Option<PathBuf>,
    write_buffer_bytes: Option<usize>,
    unbound: bool,
    /// Restrict to these interface names (`get --links`).
    only_links: Option<Vec<String>>,
}

#[derive(Subcommand)]
enum Command {
    /// Downloads one or more URLs as a single package and waits for it
    /// to finish. Ctrl-C stops cleanly (a checkpoint lands, and the
    /// package resumes on the next `get` or `resume`) rather than
    /// leaving a corrupt file.
    Get {
        urls: Vec<String>,
        /// Destination directory (default: the current directory).
        #[arg(short = 'o', long = "output", value_name = "DIR")]
        output: Option<PathBuf>,
        /// Use only these network interfaces (comma-separated names, as
        /// `nuxd links` shows them). Default: every usable link.
        #[arg(long, value_delimiter = ',', value_name = "NAMES")]
        links: Option<Vec<String>>,
    },
    /// Lists the network links (interfaces) downloads can use.
    Links,
    /// Lists every package the engine knows about.
    List,
    /// Resumes any interrupted packages and waits for them to finish.
    Resume,
}

#[tokio::main]
async fn main() -> ExitCode {
    let _ = tracing_subscriber::fmt::try_init();
    let cli = Cli::parse();
    let opts = StartOpts {
        data_dir: cli.data_dir,
        write_buffer_bytes: cli.write_buffer_bytes,
        unbound: cli.unbound,
        only_links: None,
    };
    let mut opts = opts;
    match cli.command {
        Command::Get {
            urls,
            output,
            links,
        } => {
            opts.only_links = links;
            run_get(urls, output, opts).await
        }
        Command::Links => run_links().await,
        Command::List => run_list(opts).await,
        Command::Resume => run_resume(opts).await,
    }
}

async fn start_engine(opts: StartOpts) -> Option<Engine> {
    let mut config = EngineConfig::default();
    if let Some(write_buffer_bytes) = opts.write_buffer_bytes {
        config.write_buffer_bytes = write_buffer_bytes;
    }
    let started = if opts.unbound {
        Engine::start(config, opts.data_dir).await
    } else {
        let mut setup = MonitorSetup::system();
        setup.only = opts.only_links;
        let links = LinkSetup::Monitored(setup);
        Engine::start_with(config, opts.data_dir, links).await
    };
    match started {
        Ok(engine) => Some(engine),
        Err(error) => {
            eprintln!("nuxd: couldn't start the engine: {error}");
            None
        }
    }
}

/// `nuxd links`: what discovery finds right now, with friendly labels.
async fn run_links() -> ExitCode {
    let setup = MonitorSetup::system();
    let links = match monitor::scan(&setup, &EngineConfig::default(), &[]).await {
        Ok(links) => links,
        Err(error) => {
            eprintln!("nuxd: couldn't list network interfaces: {error}");
            return ExitCode::from(2);
        }
    };
    if links.is_empty() {
        println!(
            "nuxd: no usable network links (need an up interface with a non-loopback, non-link-local address)"
        );
        return ExitCode::SUCCESS;
    }
    println!("{:<14} {:<10} {:<28} ADDRESSES", "NAME", "KIND", "LABEL");
    for link in &links {
        let addresses: Vec<String> = link
            .v4
            .map(|ip| ip.to_string())
            .into_iter()
            .chain(link.v6.map(|ip| ip.to_string()))
            .collect();
        println!(
            "{:<14} {:<10} {:<28} {}",
            link.name,
            format!("{:?}", link.kind),
            link.label,
            addresses.join(", ")
        );
    }
    ExitCode::SUCCESS
}

/// Checks `--links` against what discovery finds, so a typo is an error
/// up front instead of a download that silently waits for a link that
/// will never exist. Returns whether to go on.
async fn check_requested_links(names: &[String]) -> bool {
    let setup = MonitorSetup::system();
    let Ok(found) = monitor::scan(&setup, &EngineConfig::default(), &[]).await else {
        return true; // can't tell; let the engine try
    };
    let missing: Vec<&String> = names
        .iter()
        .filter(|name| !found.iter().any(|link| link.name == **name))
        .collect();
    if missing.len() == names.len() {
        eprintln!(
            "nuxd: none of the requested links ({}) exist — see `nuxd links`",
            names.join(", ")
        );
        return false;
    }
    for name in missing {
        eprintln!("nuxd: warning: link {name} not found; continuing without it");
    }
    true
}

async fn run_get(urls: Vec<String>, output: Option<PathBuf>, opts: StartOpts) -> ExitCode {
    if urls.is_empty() {
        eprintln!("nuxd get: at least one URL is required");
        return ExitCode::from(2);
    }
    if let Some(names) = &opts.only_links
        && !opts.unbound
        && !check_requested_links(names).await
    {
        return ExitCode::from(2);
    }
    let Some(engine) = start_engine(opts).await else {
        return ExitCode::from(2);
    };

    let dest_dir = match output {
        Some(dir) => dir,
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    let name = urls[0]
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or("download")
        .to_string();

    let ids = match engine
        .add(
            urls,
            AddOptions {
                name,
                dest_dir,
                priority: nuxd_core::model::Priority::Normal,
            },
        )
        .await
    {
        Ok(ids) => ids,
        Err(error) => {
            eprintln!("nuxd: couldn't add the package: {error}");
            engine.shutdown().await;
            return ExitCode::from(2);
        }
    };

    wait_for_packages(engine, ids).await
}

async fn run_resume(opts: StartOpts) -> ExitCode {
    let Some(engine) = start_engine(opts).await else {
        return ExitCode::from(2);
    };

    let ids: Vec<PackageId> = engine
        .snapshot()
        .await
        .packages
        .iter()
        .filter(|package| {
            !matches!(
                package.status,
                PackageStatus::Done | PackageStatus::Failed(_)
            )
        })
        .map(|package| package.id)
        .collect();

    if ids.is_empty() {
        println!("nuxd: nothing to resume");
        engine.shutdown().await;
        return ExitCode::SUCCESS;
    }

    wait_for_packages(engine, ids).await
}

async fn run_list(opts: StartOpts) -> ExitCode {
    let Some(engine) = start_engine(opts).await else {
        return ExitCode::from(2);
    };
    let snapshot = engine.snapshot().await;
    if snapshot.packages.is_empty() {
        println!("nuxd: no packages");
    }
    for package in &snapshot.packages {
        let total: u64 = package
            .files
            .iter()
            .filter_map(|file| file.expected_size)
            .sum();
        let durable: u64 = package
            .files
            .iter()
            .flat_map(|file| file.parts.iter())
            .map(|part| part.durable)
            .sum();
        println!(
            "{:>4}  {:<10?}  {:>5} file(s)  {:>10} / {:<10}  {}",
            package.id,
            package.status,
            package.files.len(),
            durable,
            if total == 0 {
                "?".to_string()
            } else {
                total.to_string()
            },
            package.name,
        );
    }
    engine.shutdown().await;
    ExitCode::SUCCESS
}

/// Watches `ids` until every one of them settles (`Done` or `Failed`),
/// printing progress bars in the meantime, or until Ctrl-C — whichever
/// comes first. Ctrl-C shuts the engine down cleanly rather than
/// aborting mid-write: whatever is durable stays durable (§9.3), and the
/// package resumes on the next `get`/`resume`.
async fn wait_for_packages(engine: Engine, ids: Vec<PackageId>) -> ExitCode {
    let mut bars = progress::Bars::new();
    let mut all_ok = true;

    loop {
        let tick = tokio::time::sleep(Duration::from_millis(250));
        tokio::select! {
            () = tick => {}
            _ = tokio::signal::ctrl_c() => {
                bars.finish_interrupted();
                println!("nuxd: interrupted — checkpointing and stopping (resume with `nuxd resume`)");
                engine.shutdown().await;
                return ExitCode::from(130);
            }
        }

        let snapshot = engine.snapshot().await;
        bars.update_links(&snapshot.links);
        let mut all_settled = true;
        for &id in &ids {
            let Some(package) = snapshot.packages.iter().find(|package| package.id == id) else {
                continue;
            };
            bars.update(package);
            match package.status {
                PackageStatus::Done => {}
                PackageStatus::Failed(_) => all_ok = false,
                _ => all_settled = false,
            }
        }
        if all_settled {
            break;
        }
    }

    bars.finish();
    engine.shutdown().await;
    if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
