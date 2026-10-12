//! `nuxd get --extract / --delete-archives` and `nuxd options` against the
//! real binary (`IMPLEMENTATION_PLAN.md` P5.5).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nuxd_testkit::{Origin, OriginOptions};
use std::io::{Cursor, Write};
use std::path::Path;
use std::sync::Arc;
use tempfile::tempdir;
use tokio::process::Command;

fn zip_bytes() -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    writer.start_file("inside/hello.txt", options).unwrap();
    writer.write_all(b"hello from the zip").unwrap();
    writer.finish().unwrap().into_inner()
}

async fn serve() -> Origin {
    Origin::start(OriginOptions {
        body: Some(Arc::new(zip_bytes())),
        content_disposition: Some("attachment; filename=\"pack.zip\"".to_string()),
        ..Default::default()
    })
    .await
}

fn nuxd(data_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nuxd"));
    command.arg("--data-dir").arg(data_dir).arg("--unbound");
    command
}

async fn run(mut command: Command) -> (bool, String, String) {
    let output = command.output().await.unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[tokio::test]
async fn extract_flag_extracts_and_keeps_the_archive() {
    let origin = serve().await;
    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let mut get = nuxd(data.path());
    // The URL's file name is what grouping sees before probing.
    get.arg("get")
        .arg(format!("http://{}/pack.zip", origin.addr()))
        .arg("-o")
        .arg(dest.path())
        .arg("--extract");
    let (ok, _, err) = run(get).await;
    assert!(ok, "{err}");
    assert_eq!(
        std::fs::read(dest.path().join("inside/hello.txt")).unwrap(),
        b"hello from the zip"
    );
    assert!(dest.path().join("pack.zip").exists());
}

#[tokio::test]
async fn delete_archives_flag_removes_the_archive_after_extraction() {
    let origin = serve().await;
    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let mut get = nuxd(data.path());
    get.arg("get")
        .arg(format!("http://{}/pack.zip", origin.addr()))
        .arg("-o")
        .arg(dest.path())
        .arg("--extract")
        .arg("--delete-archives");
    let (ok, _, err) = run(get).await;
    assert!(ok, "{err}");
    assert!(dest.path().join("inside/hello.txt").exists());
    assert!(!dest.path().join("pack.zip").exists());
}

#[tokio::test]
async fn without_flags_nothing_is_extracted_and_options_can_turn_it_on() {
    let origin = serve().await;
    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();

    let mut add = nuxd(data.path());
    add.arg("add")
        .arg(format!("http://{}/pack.zip", origin.addr()))
        .arg("-o")
        .arg(dest.path());
    let (ok, _, err) = run(add).await;
    assert!(ok, "{err}");

    let mut queue = nuxd(data.path());
    queue.arg("queue");
    let (_, out, _) = run(queue).await;
    assert!(out.contains("[archive]"), "{out}");

    let mut options = nuxd(data.path());
    options.args(["options", "1", "--extract", "true"]);
    let (ok, out, err) = run(options).await;
    assert!(ok, "{err}");
    assert!(out.contains("extract true"), "{out}");

    let mut queue = nuxd(data.path());
    queue.arg("queue");
    let (_, out, _) = run(queue).await;
    assert!(out.contains("[extract]"), "{out}");

    let mut resume = nuxd(data.path());
    resume.arg("resume");
    let (ok, _, err) = run(resume).await;
    assert!(ok, "{err}");
    assert!(dest.path().join("inside/hello.txt").exists());
}

#[tokio::test]
async fn options_on_a_plain_package_is_refused() {
    let origin = Origin::start(OriginOptions::default()).await;
    let data = tempdir().unwrap();
    let mut add = nuxd(data.path());
    add.arg("add").arg(origin.url());
    let (ok, _, _) = run(add).await;
    assert!(ok);
    let mut options = nuxd(data.path());
    options.args(["options", "1", "--extract", "true"]);
    let (ok, _, err) = run(options).await;
    assert!(!ok);
    assert!(err.contains("doesn't look like an archive"), "{err}");
}
