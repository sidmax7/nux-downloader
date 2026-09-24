//! `nuxd`: thin CLI over `nuxd-core` (`ARCHITECTURE.md` §15). The primary
//! way to exercise the engine before the Tauri UI exists (Phase 6).

mod manage;
mod progress;

use clap::{Parser, Subcommand, ValueEnum};
use nuxd_core::config::EngineConfig;
use nuxd_core::model::{FileId, FileStatus, PackageId, PackageStatus, Priority};
use nuxd_core::net::monitor::{self, LinkSetup, MonitorSetup};
use nuxd_core::{AddOptions, Engine, Target};
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
    /// Limit connections to a host: `PATTERN=CONNS[:FILES]` — CONNS
    /// connections per link and, optionally, at most FILES files active at
    /// once; `*` matches any run of characters (`*.example.com=4:2`).
    /// Repeatable. Applies to commands that download.
    #[arg(
        long = "host-limit",
        global = true,
        value_name = "PATTERN=CONNS[:FILES]"
    )]
    host_limits: Vec<String>,
}

/// Priority levels as typed on the command line.
#[derive(Clone, Copy, ValueEnum)]
enum PriorityArg {
    Low,
    Normal,
    High,
}

impl From<PriorityArg> for Priority {
    fn from(arg: PriorityArg) -> Self {
        match arg {
            PriorityArg::Low => Self::Low,
            PriorityArg::Normal => Self::Normal,
            PriorityArg::High => Self::High,
        }
    }
}

/// The global options that shape how the engine starts.
struct StartOpts {
    data_dir: Option<PathBuf>,
    write_buffer_bytes: Option<usize>,
    unbound: bool,
    host_limits: Vec<String>,
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
    /// Adds URLs to the queue as one package without downloading them.
    /// URLs come from the arguments, `-i FILE` (`-` for stdin), or stdin
    /// when it is piped. Run `nuxd resume` to download the queue.
    Add {
        urls: Vec<String>,
        /// A file with one link per line (`#` comments and blank lines are
        /// skipped); `-` reads stdin.
        #[arg(short = 'i', long = "input", value_name = "FILE")]
        input: Option<PathBuf>,
        /// Destination directory (default: the current directory).
        #[arg(short = 'o', long = "output", value_name = "DIR")]
        output: Option<PathBuf>,
        /// Package name (default: the first file's name).
        #[arg(long)]
        name: Option<String>,
        #[arg(long, value_enum, default_value = "normal")]
        priority: PriorityArg,
    },
    /// Lists the queue: every package with its priority, status and
    /// progress (`-v` adds each file).
    #[command(alias = "list")]
    Queue {
        #[arg(short, long)]
        verbose: bool,
    },
    /// Pauses a package (its id from `nuxd queue`) or `all`.
    Pause { target: String },
    /// Downloads the queue: resumes the given packages (or every paused or
    /// interrupted one) and waits for them to finish.
    Resume { targets: Vec<String> },
    /// Removes a package (id) or `all` from the queue; `--delete-files`
    /// also deletes what was downloaded.
    Remove {
        target: String,
        #[arg(long)]
        delete_files: bool,
    },
    /// Sets a package's priority.
    Priority {
        id: u64,
        #[arg(value_enum)]
        level: PriorityArg,
    },
    /// Moves a package in the queue to `top`, `bottom`, or a 1-based
    /// position.
    Move { id: u64, position: String },
    /// Starts a failed file over from scratch (its id from
    /// `nuxd queue -v`); `nuxd resume` then downloads it again.
    RestartFile { file: u64 },
}

#[tokio::main]
async fn main() -> ExitCode {
    // `RUST_LOG=nuxd_core=debug nuxd get … 2>log.txt` shows every failed
    // attempt and host backoff; the default shows warnings only.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
    let cli = Cli::parse();
    let opts = StartOpts {
        data_dir: cli.data_dir,
        write_buffer_bytes: cli.write_buffer_bytes,
        unbound: cli.unbound,
        host_limits: cli.host_limits,
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
        Command::Add {
            urls,
            input,
            output,
            name,
            priority,
        } => manage::add(urls, input, output, name, priority.into(), opts.data_dir).await,
        Command::Queue { verbose } => manage::queue(verbose, opts.data_dir).await,
        Command::Pause { target } => match manage::parse_target(&target) {
            Ok(target) => manage::pause(target, opts.data_dir).await,
            Err(error) => usage(&error),
        },
        Command::Resume { targets } => run_resume(targets, opts).await,
        Command::Remove {
            target,
            delete_files,
        } => match manage::parse_target(&target) {
            Ok(target) => manage::remove(target, delete_files, opts.data_dir).await,
            Err(error) => usage(&error),
        },
        Command::Priority { id, level } => {
            manage::set_priority(PackageId::new(id), level.into(), opts.data_dir).await
        }
        Command::Move { id, position } => match manage::parse_position(&position) {
            Ok(position) => manage::move_to(PackageId::new(id), position, opts.data_dir).await,
            Err(error) => usage(&error),
        },
        Command::RestartFile { file } => {
            manage::restart_file(FileId::new(file), opts.data_dir).await
        }
    }
}

fn usage(message: &str) -> ExitCode {
    eprintln!("nuxd: {message}");
    ExitCode::from(2)
}

async fn start_engine(opts: StartOpts) -> Option<Engine> {
    let mut config = EngineConfig::default();
    if let Some(write_buffer_bytes) = opts.write_buffer_bytes {
        config.write_buffer_bytes = write_buffer_bytes;
    }
    for rule in &opts.host_limits {
        if let Err(error) = config.hosts.add_rule_text(rule) {
            eprintln!("nuxd: --host-limit: {error}");
            return None;
        }
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

async fn run_resume(targets: Vec<String>, opts: StartOpts) -> ExitCode {
    let mut wanted = Vec::new();
    for text in &targets {
        match manage::parse_target(text) {
            Ok(target) => wanted.push(target),
            Err(error) => return usage(&error),
        }
    }
    let Some(engine) = start_engine(opts).await else {
        return ExitCode::from(2);
    };
    // No targets means the whole queue: un-pause everything, and anything
    // that was mid-download is already running again.
    if wanted.is_empty() {
        wanted.push(Target::All);
    }
    for target in wanted {
        if let Err(error) = engine.resume(target).await {
            eprintln!("nuxd resume: {error}");
        }
    }

    let ids: Vec<PackageId> = engine
        .snapshot()
        .await
        .packages
        .iter()
        .filter(|package| {
            !matches!(
                package.status,
                PackageStatus::Done | PackageStatus::Failed(_) | PackageStatus::Paused
            )
        })
        .map(|package| package.id)
        .collect();

    if ids.is_empty() {
        println!("nuxd: nothing to download");
        engine.shutdown().await;
        return ExitCode::SUCCESS;
    }

    wait_for_packages(engine, ids).await
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
    // Say why, not just "failed".
    let snapshot = engine.snapshot().await;
    for package in snapshot.packages.iter().filter(|p| ids.contains(&p.id)) {
        for file in &package.files {
            if let FileStatus::Failed(error) = &file.status {
                eprintln!("nuxd: {}: {error}", file.name);
            }
        }
        if let PackageStatus::Failed(error) = &package.status {
            eprintln!("nuxd: package {}: {error}", package.name);
        }
    }
    engine.shutdown().await;
    if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
