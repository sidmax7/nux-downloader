//! The queue-management commands against the real binary
//! (`IMPLEMENTATION_PLAN.md` P4.7): `add` and friends never download or
//! probe as a side effect, and `resume` does the work.

use nuxd_testkit::{Origin, OriginOptions, content_range};
use std::process::Stdio;
use tempfile::tempdir;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

fn nuxd(data_dir: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nuxd"));
    command.arg("--data-dir").arg(data_dir).arg("--unbound");
    command
}

async fn run(mut command: Command) -> (bool, String, String) {
    let output = command
        .output()
        .await
        .unwrap_or_else(|e| panic!("couldn't run nuxd: {e}"));
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

async fn queue_text(data_dir: &std::path::Path) -> String {
    let mut command = nuxd(data_dir);
    command.arg("queue").arg("-v");
    let (ok, out, err) = run(command).await;
    assert!(ok, "queue failed: {err}");
    out
}

#[tokio::test]
async fn managing_the_queue_never_touches_the_network_and_resume_downloads() {
    let size = 6000;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 91,
        ..Default::default()
    })
    .await;
    let data_dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));

    // add: a link file with comments and blank lines, plus an argument.
    let links = data_dir.path().join("links.txt");
    std::fs::write(
        &links,
        format!("# my files\n{}\n\n{}\n", origin.url(), origin.url()),
    )
    .unwrap_or_else(|e| panic!("write links: {e}"));
    let mut add = nuxd(data_dir.path());
    add.arg("add")
        .arg(origin.url())
        .arg("-i")
        .arg(&links)
        .arg("-o")
        .arg(dest.path())
        .args(["--name", "batch", "--priority", "low"]);
    let (ok, out, err) = run(add).await;
    assert!(ok, "add failed: {err}");
    assert!(out.contains("added package 1"), "{out}");
    assert!(out.contains("3 file(s)"), "{out}");

    // A second package, so ordering has something to act on.
    let mut add2 = nuxd(data_dir.path());
    add2.arg("add").arg(origin.url()).arg("-o").arg(dest.path());
    assert!(run(add2).await.0);

    let text = queue_text(data_dir.path()).await;
    assert!(text.contains("batch"), "{text}");
    assert!(text.contains("Low"), "{text}");
    assert_eq!(text.matches("file ").count(), 4, "{text}");

    // Nothing was probed or downloaded by adding and listing.
    assert!(
        origin.requests().is_empty(),
        "management touched the network"
    );
    assert_eq!(
        std::fs::read_dir(dest.path())
            .unwrap_or_else(|e| panic!("read_dir: {e}"))
            .count(),
        0
    );

    // priority, move, pause, remove.
    let mut priority = nuxd(data_dir.path());
    priority.args(["priority", "1", "high"]);
    assert!(run(priority).await.0);
    let mut mv = nuxd(data_dir.path());
    mv.args(["move", "2", "top"]);
    assert!(run(mv).await.0);
    let text = queue_text(data_dir.path()).await;
    let lines: Vec<&str> = text.lines().collect();
    let first_package_line = lines
        .iter()
        .find(|l| l.trim_start().starts_with(char::is_numeric))
        .unwrap_or_else(|| panic!("no package line: {text}"));
    assert!(
        first_package_line.trim_start().starts_with('2'),
        "package 2 should now be first: {text}"
    );
    assert!(text.contains("High"), "{text}");

    let mut pause = nuxd(data_dir.path());
    pause.args(["pause", "1"]);
    assert!(run(pause).await.0);
    assert!(queue_text(data_dir.path()).await.contains("paused"));

    // resume downloads: package 1 un-paused, package 2 waiting.
    let mut resume = nuxd(data_dir.path());
    resume.arg("resume");
    let (ok, out, err) = run(resume).await;
    assert!(ok, "resume failed: {out}\n{err}");
    let mut finished = 0;
    for entry in std::fs::read_dir(dest.path()).unwrap_or_else(|e| panic!("read_dir: {e}")) {
        let path = entry.unwrap_or_else(|e| panic!("entry: {e}")).path();
        if path.extension().is_some_and(|x| x == "nuxdpart") {
            continue;
        }
        assert_eq!(
            std::fs::read(&path).unwrap_or_else(|e| panic!("read: {e}")),
            content_range(91, 0, size)
        );
        finished += 1;
    }
    assert_eq!(finished, 4, "all four files downloaded");

    let mut remove = nuxd(data_dir.path());
    remove.args(["remove", "all"]);
    assert!(run(remove).await.0);
    assert!(queue_text(data_dir.path()).await.contains("queue is empty"));
    origin.shutdown().await;
}

#[tokio::test]
async fn add_reads_links_from_stdin() {
    let data_dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let mut command = nuxd(data_dir.path());
    command
        .arg("add")
        .arg("-o")
        .arg(dest.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap_or_else(|e| panic!("spawn: {e}"));
    let mut stdin = child.stdin.take().unwrap_or_else(|| panic!("no stdin"));
    stdin
        .write_all(b"http://127.0.0.1:9/a.bin\nhttp://127.0.0.1:9/b.bin\n")
        .await
        .unwrap_or_else(|e| panic!("write: {e}"));
    drop(stdin);
    let output = child
        .wait_with_output()
        .await
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("2 file(s)"));
}

#[tokio::test]
async fn bad_arguments_are_usage_errors() {
    let data_dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    for args in [
        vec!["pause", "seven"],
        vec!["move", "1", "zero"],
        vec!["--host-limit", "nonsense", "resume"],
    ] {
        let mut command = nuxd(data_dir.path());
        command.args(&args);
        let (ok, _, err) = run(command).await;
        assert!(!ok, "{args:?} should fail");
        assert!(!err.is_empty(), "{args:?} should explain why");
    }
}
