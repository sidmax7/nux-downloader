//! `nuxd`: thin CLI over `nuxd-core` (`ARCHITECTURE.md` §15). The primary
//! way to exercise the engine before the Tauri UI exists (Phase 6).

mod progress;

use clap::{Parser, Subcommand};
use nuxd_core::config::EngineConfig;
use nuxd_core::model::{PackageId, PackageStatus};
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
    },
    /// Lists every package the engine knows about.
    List,
    /// Resumes any interrupted packages and waits for them to finish.
    Resume,
}

#[tokio::main]
async fn main() -> ExitCode {
    let _ = tracing_subscriber::fmt::try_init();
    let cli = Cli::parse();
    match cli.command {
        Command::Get { urls, output } => {
            run_get(urls, output, cli.data_dir, cli.write_buffer_bytes).await
        }
        Command::List => run_list(cli.data_dir, cli.write_buffer_bytes).await,
        Command::Resume => run_resume(cli.data_dir, cli.write_buffer_bytes).await,
    }
}

async fn start_engine(
    data_dir: Option<PathBuf>,
    write_buffer_bytes: Option<usize>,
) -> Option<Engine> {
    let mut config = EngineConfig::default();
    if let Some(write_buffer_bytes) = write_buffer_bytes {
        config.write_buffer_bytes = write_buffer_bytes;
    }
    match Engine::start(config, data_dir).await {
        Ok(engine) => Some(engine),
        Err(error) => {
            eprintln!("nuxd: couldn't start the engine: {error}");
            None
        }
    }
}

async fn run_get(
    urls: Vec<String>,
    output: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    write_buffer_bytes: Option<usize>,
) -> ExitCode {
    if urls.is_empty() {
        eprintln!("nuxd get: at least one URL is required");
        return ExitCode::from(2);
    }
    let Some(engine) = start_engine(data_dir, write_buffer_bytes).await else {
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

async fn run_resume(data_dir: Option<PathBuf>, write_buffer_bytes: Option<usize>) -> ExitCode {
    let Some(engine) = start_engine(data_dir, write_buffer_bytes).await else {
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

async fn run_list(data_dir: Option<PathBuf>, write_buffer_bytes: Option<usize>) -> ExitCode {
    let Some(engine) = start_engine(data_dir, write_buffer_bytes).await else {
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
