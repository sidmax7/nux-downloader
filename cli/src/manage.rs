//! Queue-management commands (`nuxd add`, `queue`, `pause`, `remove`,
//! `priority`, `move`, `restart-file`). They open the queue *offline*
//! (`Engine::open_offline`): nothing is probed or downloaded as a side
//! effect, and only what a command touches is rewritten. Downloading is
//! `nuxd resume` (or `nuxd get`).

use nuxd_core::config::EngineConfig;
use nuxd_core::model::{FileId, FileStatus, PackageId, PackageStatus, Priority};
use nuxd_core::{AddOptions, Engine, Target};
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;

/// Link lists as people write them: one per line, or several separated by
/// whitespace; blank lines and `#` comments are ignored.
pub fn parse_links(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .flat_map(str::split_whitespace)
        .map(str::to_string)
        .collect()
}

/// URLs from the arguments, then from `-i FILE` (`-` = stdin), and — when
/// nothing else was given and stdin is piped — from stdin.
pub fn gather_inputs(urls: Vec<String>, input: Option<PathBuf>) -> Result<Vec<String>, String> {
    let mut all = urls;
    match input {
        Some(path) if path.as_os_str() == "-" => all.extend(parse_links(&read_stdin()?)),
        Some(path) => {
            let text = std::fs::read_to_string(&path)
                .map_err(|error| format!("couldn't read {}: {error}", path.display()))?;
            all.extend(parse_links(&text));
        }
        None if all.is_empty() && !std::io::stdin().is_terminal() => {
            all.extend(parse_links(&read_stdin()?));
        }
        None => {}
    }
    Ok(all)
}

fn read_stdin() -> Result<String, String> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .map_err(|error| format!("couldn't read stdin: {error}"))?;
    Ok(text)
}

async fn open(data_dir: Option<PathBuf>) -> Option<Engine> {
    match Engine::open_offline(EngineConfig::default(), data_dir).await {
        Ok(engine) => Some(engine),
        Err(error) => {
            eprintln!("nuxd: couldn't open the queue: {error}");
            None
        }
    }
}

/// `all`, or a package id.
pub fn parse_target(text: &str) -> Result<Target, String> {
    if text.eq_ignore_ascii_case("all") {
        return Ok(Target::All);
    }
    text.parse::<u64>()
        .map(|id| Target::Package(PackageId::new(id)))
        .map_err(|_| format!("expected a package id or `all`, got `{text}`"))
}

pub async fn add(
    urls: Vec<String>,
    input: Option<PathBuf>,
    output: Option<PathBuf>,
    name: Option<String>,
    priority: Priority,
    post: crate::PostFlags,
    data_dir: Option<PathBuf>,
) -> ExitCode {
    let inputs = match gather_inputs(urls, input) {
        Ok(inputs) => inputs,
        Err(error) => {
            eprintln!("nuxd add: {error}");
            return ExitCode::from(2);
        }
    };
    if inputs.is_empty() {
        eprintln!("nuxd add: give URLs as arguments, with -i FILE, or on stdin");
        return ExitCode::from(2);
    }
    let Some(engine) = open(data_dir).await else {
        return ExitCode::from(2);
    };
    let dest_dir =
        output.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let name = name.unwrap_or_else(|| crate::default_package_name(&inputs[0]));
    let count = inputs.len();
    let result = engine
        .add(
            inputs,
            AddOptions {
                name,
                dest_dir,
                priority,
                extract: post.extract,
                delete_archives: post.delete_archives,
            },
        )
        .await;
    let code = match result {
        Ok(ids) => {
            for id in ids {
                println!("added package {id} ({count} file(s)); run `nuxd resume` to download");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("nuxd add: {error}");
            ExitCode::from(2)
        }
    };
    engine.shutdown().await;
    code
}

/// Sets or clears a package's speed limit.
pub async fn limit(
    id: PackageId,
    rate: Option<nuxd_core::model::ByteRate>,
    data_dir: Option<PathBuf>,
) -> ExitCode {
    let Some(engine) = open(data_dir).await else {
        return ExitCode::from(2);
    };
    if !engine.snapshot().await.packages.iter().any(|p| p.id == id) {
        eprintln!("nuxd limit: no package {id}");
        engine.shutdown().await;
        return ExitCode::from(2);
    }
    let code = match engine.set_package_limit(id, rate).await {
        Ok(()) => {
            match rate {
                Some(rate) => println!("package {id}: limited to {} bytes/s", rate.bytes_per_sec()),
                None => println!("package {id}: no speed limit"),
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("nuxd limit: {error}");
            ExitCode::from(2)
        }
    };
    engine.shutdown().await;
    code
}

/// Changes a package's extraction options.
pub async fn options(
    id: PackageId,
    extract: Option<bool>,
    delete_archives: Option<bool>,
    data_dir: Option<PathBuf>,
) -> ExitCode {
    let Some(engine) = open(data_dir).await else {
        return ExitCode::from(2);
    };
    let snapshot = engine.snapshot().await;
    let Some(package) = snapshot.packages.iter().find(|p| p.id == id) else {
        eprintln!("nuxd options: no package {id}");
        engine.shutdown().await;
        return ExitCode::from(2);
    };
    if package.archive.is_none() {
        eprintln!("nuxd options: package {id} doesn't look like an archive; nothing to extract");
        engine.shutdown().await;
        return ExitCode::from(2);
    }
    let extract = extract.unwrap_or(package.extract);
    let delete_archives = delete_archives.unwrap_or(package.delete_archives);
    let result = engine.set_post_options(id, extract, delete_archives).await;
    let code = match result {
        Ok(()) => {
            println!("package {id}: extract {extract}, delete archives {delete_archives}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("nuxd options: {error}");
            ExitCode::from(2)
        }
    };
    engine.shutdown().await;
    code
}

pub async fn queue(verbose: bool, data_dir: Option<PathBuf>) -> ExitCode {
    let Some(engine) = open(data_dir).await else {
        return ExitCode::from(2);
    };
    let snapshot = engine.snapshot().await;
    if snapshot.packages.is_empty() {
        println!("nuxd: the queue is empty");
    } else {
        println!(
            "{:>4} {:>3}  {:<8} {:<20} {:>6}  {:>21}  NAME",
            "ID", "POS", "PRIORITY", "STATUS", "FILES", "PROGRESS"
        );
    }
    for (position, package) in snapshot.packages.iter().enumerate() {
        let total: u64 = package.files.iter().filter_map(|f| f.expected_size).sum();
        let durable: u64 = package
            .files
            .iter()
            .flat_map(|f| f.parts.iter())
            .map(|p| p.durable)
            .sum();
        let progress = if total == 0 {
            format!("{durable} / ?")
        } else {
            format!("{durable} / {total}")
        };
        println!(
            "{:>4} {:>3}  {:<8} {:<20} {:>6}  {:>21}  {}",
            package.id,
            position + 1,
            format!("{:?}", package.priority),
            status_text(package),
            package.files.len(),
            progress,
            name_with_options(package),
        );
        if verbose {
            for file in &package.files {
                let done: u64 = file.parts.iter().map(|p| p.durable).sum();
                let size = file
                    .expected_size
                    .map_or_else(|| "?".to_string(), |s| s.to_string());
                println!(
                    "       file {:>4}  {:<12} {:>12} / {:<12} {}",
                    file.id,
                    file_status_text(&file.status),
                    done,
                    size,
                    file.name
                );
            }
        }
    }
    engine.shutdown().await;
    ExitCode::SUCCESS
}

/// The package name, tagged with what happens to its archive.
fn name_with_options(package: &nuxd_core::model::Package) -> String {
    match (
        package.archive.is_some(),
        package.extract,
        package.delete_archives,
    ) {
        (false, ..) => package.name.clone(),
        (true, false, _) => format!("{} [archive]", package.name),
        (true, true, false) => format!("{} [extract]", package.name),
        (true, true, true) => format!("{} [extract, delete archives]", package.name),
    }
}

fn status_text(package: &nuxd_core::model::Package) -> String {
    match (&package.status, &package.last_error) {
        (PackageStatus::Failed(error), _) => format!("failed: {error}"),
        (PackageStatus::Paused, Some(error)) => format!("paused: {error}"),
        (other, _) => format!("{other:?}").to_lowercase(),
    }
}

fn file_status_text(status: &FileStatus) -> String {
    match status {
        FileStatus::Failed(error) => format!("failed: {error}"),
        other => format!("{other:?}").to_lowercase(),
    }
}

/// Runs `action` on the offline queue and reports failure uniformly.
async fn with_queue<F, Fut>(data_dir: Option<PathBuf>, what: &str, action: F) -> ExitCode
where
    F: FnOnce(Engine) -> Fut,
    Fut: std::future::Future<Output = (Engine, Result<(), nuxd_core::error::EngineError>)>,
{
    let Some(engine) = open(data_dir).await else {
        return ExitCode::from(2);
    };
    let (engine, result) = action(engine).await;
    let code = match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nuxd {what}: {error}");
            ExitCode::from(1)
        }
    };
    engine.shutdown().await;
    code
}

pub async fn pause(target: Target, data_dir: Option<PathBuf>) -> ExitCode {
    with_queue(data_dir, "pause", |engine| async move {
        let result = engine.pause(target).await;
        (engine, result)
    })
    .await
}

pub async fn remove(target: Target, delete_files: bool, data_dir: Option<PathBuf>) -> ExitCode {
    with_queue(data_dir, "remove", |engine| async move {
        let result = engine.remove(target, delete_files).await;
        (engine, result)
    })
    .await
}

pub async fn set_priority(
    id: PackageId,
    priority: Priority,
    data_dir: Option<PathBuf>,
) -> ExitCode {
    with_queue(data_dir, "priority", |engine| async move {
        let result = engine.set_priority(id, priority).await;
        (engine, result)
    })
    .await
}

/// `top`, `bottom`, or a 1-based position.
pub fn parse_position(text: &str) -> Result<usize, String> {
    match text.to_ascii_lowercase().as_str() {
        "top" | "first" => Ok(0),
        "bottom" | "last" => Ok(usize::MAX),
        other => other
            .parse::<usize>()
            .ok()
            .filter(|n| *n >= 1)
            .map(|n| n - 1)
            .ok_or_else(|| format!("expected `top`, `bottom` or a position ≥ 1, got `{text}`")),
    }
}

pub async fn move_to(id: PackageId, position: usize, data_dir: Option<PathBuf>) -> ExitCode {
    with_queue(data_dir, "move", |engine| async move {
        let result = engine.move_to(id, position).await;
        (engine, result)
    })
    .await
}

pub async fn restart_file(file: FileId, data_dir: Option<PathBuf>) -> ExitCode {
    with_queue(data_dir, "restart-file", |engine| async move {
        let result = engine.restart_file(file).await;
        (engine, result)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_are_read_one_per_line_or_whitespace_separated() {
        let text = "\n# my links\nhttp://a/1  http://a/2\n   http://b/3   \n\n#http://skipped\n";
        assert_eq!(
            parse_links(text),
            ["http://a/1", "http://a/2", "http://b/3"]
        );
    }

    #[test]
    fn targets_are_all_or_a_number() {
        assert!(matches!(parse_target("all"), Ok(Target::All)));
        assert!(matches!(parse_target("ALL"), Ok(Target::All)));
        assert!(matches!(
            parse_target("7"),
            Ok(Target::Package(id)) if id == PackageId::new(7)
        ));
        assert!(parse_target("seven").is_err());
    }

    #[test]
    fn positions_are_one_based_with_top_and_bottom() {
        assert_eq!(parse_position("top"), Ok(0));
        assert_eq!(parse_position("1"), Ok(0));
        assert_eq!(parse_position("3"), Ok(2));
        assert_eq!(parse_position("bottom"), Ok(usize::MAX));
        assert!(parse_position("0").is_err());
        assert!(parse_position("x").is_err());
    }
}
