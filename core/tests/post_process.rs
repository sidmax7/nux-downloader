//! Verification and extraction through the whole engine (`IMPLEMENTATION_PLAN.md`
//! P5.5): archives are served by testkit origins, grouped by `collect`,
//! downloaded, verified and extracted, with the two per-package options
//! (`extract`, `delete_archives`) honoured independently.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nuxd_core::config::EngineConfig;
use nuxd_core::engine::sim::one_link;
use nuxd_core::model::{ArchiveScheme, FileStatus, PackageError, PackageStatus, Priority};
use nuxd_core::net::bind::BindPolicy;
use nuxd_core::net::monitor::LinkSetup;
use nuxd_core::{AddOptions, Engine};
use nuxd_testkit::{Origin, OriginOptions};
use std::io::{Cursor, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;

async fn engine(data_dir: &Path) -> Engine {
    Engine::start_with(
        EngineConfig::default(),
        Some(data_dir.to_path_buf()),
        LinkSetup::Fixed {
            links: vec![one_link()],
            bind: BindPolicy::Off,
        },
    )
    .await
    .unwrap_or_else(|e| panic!("start: {e}"))
}

/// An origin serving `bytes` under a Content-Disposition `name`.
async fn serve(name: &str, bytes: Vec<u8>) -> Origin {
    Origin::start(OriginOptions {
        body: Some(Arc::new(bytes)),
        content_disposition: Some(format!("attachment; filename=\"{name}\"")),
        ..Default::default()
    })
    .await
}

fn pseudo(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    (0..len)
        .map(|_| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 33) as u8
        })
        .collect()
}

fn payload() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("readme.txt", b"extracted by nuxd\n".to_vec()),
        ("data/blob.bin", pseudo(250_000, 5)),
    ]
}

fn make_7z(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut writer = sevenz_rust2::ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
    for (name, data) in entries {
        writer
            .push_archive_entry(
                sevenz_rust2::ArchiveEntry::new_file(name),
                Some(Cursor::new(data.clone())),
            )
            .unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn make_zip(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    for (name, data) in entries {
        writer.start_file(*name, options).unwrap();
        writer.write_all(data).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

/// Splits `bytes` into `parts` volumes served as `<base>.001`, ….
async fn serve_volumes(base: &str, bytes: &[u8], parts: usize) -> Vec<Origin> {
    let chunk = bytes.len().div_ceil(parts);
    let mut origins = Vec::new();
    for (i, part) in bytes.chunks(chunk).enumerate() {
        origins.push(serve(&format!("{base}.{:03}", i + 1), part.to_vec()).await);
    }
    origins
}

fn opts(dest: &Path, extract: bool, delete: bool) -> AddOptions {
    AddOptions {
        name: "pkg".to_string(),
        dest_dir: dest.to_path_buf(),
        priority: Priority::Normal,
        extract,
        delete_archives: delete,
    }
}

async fn wait_status(engine: &Engine, want: impl Fn(&PackageStatus) -> bool) -> PackageStatus {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let snapshot = engine.snapshot().await;
            if let Some(package) = snapshot.packages.first()
                && want(&package.status)
            {
                return package.status.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the package never reached the expected status")
}

fn assert_extracted(dest: &Path) {
    for (name, data) in payload() {
        assert_eq!(std::fs::read(dest.join(name)).unwrap(), data, "{name}");
    }
}

fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn collect_groups_a_split_archive_and_extract_leaves_the_volumes() {
    let origins = serve_volumes("set.7z", &make_7z(&payload()), 3).await;
    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let engine = engine(data.path()).await;

    let urls: Vec<String> = origins.iter().rev().map(Origin::url).collect();
    let drafts = engine.collect(urls).await;
    assert_eq!(drafts.len(), 1, "{drafts:?}");
    let draft = drafts.into_iter().next().unwrap();
    let archive = draft.archive.clone().expect("recognised as an archive");
    assert_eq!(archive.scheme, ArchiveScheme::SevenZip);
    assert!(draft.offers_extract());
    assert!(draft.missing_volumes.is_empty());
    // Volume order, whatever order they were pasted in.
    let names: Vec<_> = draft
        .files
        .iter()
        .map(|f| f.probe.as_ref().unwrap().suggested_name.clone())
        .collect();
    assert_eq!(names, vec!["set.7z.001", "set.7z.002", "set.7z.003"]);

    // The shell pre-ticks "extract"; "delete" is off.
    let options = AddOptions::for_draft(&draft, dest.path().to_path_buf());
    assert!(options.extract && !options.delete_archives);
    engine.enqueue(draft, options).await.unwrap();

    wait_status(&engine, |s| *s == PackageStatus::Done).await;
    assert_extracted(dest.path());
    // Extraction doesn't delete the archives unless asked.
    let files = listing(dest.path());
    assert!(files.contains(&"set.7z.001".to_string()), "{files:?}");
    assert!(files.contains(&"set.7z.003".to_string()), "{files:?}");
    assert!(
        files.iter().all(|f| !f.contains("nuxdextract")),
        "{files:?}"
    );
    engine.shutdown().await;
}

#[tokio::test]
async fn delete_archives_removes_only_the_volumes_after_a_good_extraction() {
    let origins = serve_volumes("set.7z", &make_7z(&payload()), 2).await;
    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let engine = engine(data.path()).await;
    let drafts = engine
        .collect(origins.iter().map(Origin::url).collect())
        .await;
    let draft = drafts.into_iter().next().unwrap();
    engine
        .enqueue(draft, opts(dest.path(), true, true))
        .await
        .unwrap();
    wait_status(&engine, |s| *s == PackageStatus::Done).await;
    // The deletion runs just after Done is announced.
    tokio::time::timeout(Duration::from_secs(10), async {
        while listing(dest.path()).iter().any(|f| f.starts_with("set.7z")) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the archives were never deleted");
    assert_extracted(dest.path());
    engine.shutdown().await;
}

#[tokio::test]
async fn without_the_extract_option_the_archive_is_left_alone() {
    let origins = serve_volumes("set.7z", &make_7z(&payload()), 2).await;
    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let engine = engine(data.path()).await;
    let drafts = engine
        .collect(origins.iter().map(Origin::url).collect())
        .await;
    let draft = drafts.into_iter().next().unwrap();
    // Not ticked; but "delete" is irrelevant without extraction.
    engine
        .enqueue(draft, opts(dest.path(), false, true))
        .await
        .unwrap();
    wait_status(&engine, |s| *s == PackageStatus::Done).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        listing(dest.path()),
        vec!["set.7z.001".to_string(), "set.7z.002".to_string()]
    );
    engine.shutdown().await;
}

#[tokio::test]
async fn the_options_can_be_switched_on_while_downloading() {
    let origin = serve("one.zip", make_zip(&payload())).await;
    // Hold the transfer so there is time to change our mind. Request 1 is
    // `collect`'s probe; the transfer is request 2. (Holding "the next
    // request" caught the probe, and a fast machine finished the file
    // before the options changed.)
    origin.hold_request_at(2, 10);
    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let engine = engine(data.path()).await;
    let drafts = engine.collect(vec![origin.url()]).await;
    let draft = drafts.into_iter().next().unwrap();
    assert!(draft.offers_extract(), "a lone zip is an archive too");
    let package = engine
        .enqueue(draft, opts(dest.path(), false, false))
        .await
        .unwrap();
    engine.set_post_options(package, true, false).await.unwrap();
    origin.release();
    wait_status(&engine, |s| *s == PackageStatus::Done).await;
    assert_extracted(dest.path());
    engine.shutdown().await;
}

#[tokio::test]
async fn a_hostile_archive_fails_the_package_and_keeps_the_archive() {
    let evil = make_zip(&[
        ("fine.txt", b"ok".to_vec()),
        ("../escaped.txt", b"x".to_vec()),
    ]);
    let origin = serve("evil.zip", evil).await;
    let data = tempdir().unwrap();
    let outer = tempdir().unwrap();
    let dest = outer.path().join("dest");
    std::fs::create_dir(&dest).unwrap();
    let engine = engine(data.path()).await;
    let drafts = engine.collect(vec![origin.url()]).await;
    let draft = drafts.into_iter().next().unwrap();
    // Even with "delete archives" on, a failed extraction deletes nothing.
    engine
        .enqueue(draft, opts(&dest, true, true))
        .await
        .unwrap();
    let status = wait_status(&engine, |s| matches!(s, PackageStatus::Failed(_))).await;
    assert!(
        matches!(status, PackageStatus::Failed(PackageError::ExtractFailed { ref detail }) if detail.contains("unsafe")),
        "{status:?}"
    );
    assert!(!outer.path().join("escaped.txt").exists());
    assert!(
        !dest.join("fine.txt").exists(),
        "partial extraction left behind"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(listing(&dest), vec!["evil.zip".to_string()]);
    engine.shutdown().await;
}

#[tokio::test]
async fn a_checksum_sidecar_is_verified_and_a_bad_file_is_caught() {
    let good = pseudo(40_000, 11);
    let crc = crc32fast::hash(&good);
    let sfv_good = format!("data.bin {crc:08X}\n").into_bytes();
    let sfv_bad = format!("data.bin {:08X}\n", crc ^ 1).into_bytes();

    // A good one passes.
    {
        let a = serve("data.bin", good.clone()).await;
        let b = serve("data.sfv", sfv_good).await;
        let data = tempdir().unwrap();
        let dest = tempdir().unwrap();
        let engine = engine(data.path()).await;
        let drafts = engine.collect(vec![a.url(), b.url()]).await;
        assert_eq!(drafts.len(), 1);
        engine
            .enqueue(
                drafts.into_iter().next().unwrap(),
                opts(dest.path(), false, false),
            )
            .await
            .unwrap();
        wait_status(&engine, |s| *s == PackageStatus::Done).await;
        engine.shutdown().await;
    }

    // A bad one is caught: the package fails, the file is marked and its
    // bytes are deleted so a restart fetches them afresh.
    {
        let a = serve("data.bin", good.clone()).await;
        let b = serve("data.sfv", sfv_bad).await;
        let data = tempdir().unwrap();
        let dest = tempdir().unwrap();
        let engine = engine(data.path()).await;
        let drafts = engine.collect(vec![a.url(), b.url()]).await;
        engine
            .enqueue(
                drafts.into_iter().next().unwrap(),
                opts(dest.path(), false, false),
            )
            .await
            .unwrap();
        let status = wait_status(&engine, |s| matches!(s, PackageStatus::Failed(_))).await;
        assert!(
            matches!(status, PackageStatus::Failed(PackageError::VerifyFailed { ref detail }) if detail.contains("data.bin")),
            "{status:?}"
        );
        let snapshot = engine.snapshot().await;
        let files = &snapshot.packages[0].files;
        let bad = files.iter().find(|f| f.name == "data.bin").unwrap();
        assert!(
            matches!(bad.status, FileStatus::Failed(_)),
            "{:?}",
            bad.status
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while dest.path().join("data.bin").exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the corrupt file was never deleted");
        engine.shutdown().await;
    }
}

#[tokio::test]
async fn add_groups_archive_urls_into_their_own_packages() {
    let origins = serve_volumes("b.7z", &make_7z(&payload()), 2).await;
    let note = serve("note.txt", b"just a note".to_vec()).await;
    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let engine = engine(data.path()).await;

    // `add` doesn't probe, so grouping works from the URL file names.
    let mut sources: Vec<String> = origins
        .iter()
        .enumerate()
        .map(|(i, o)| format!("http://{}/b.7z.{:03}", o.addr(), i + 1))
        .collect();
    sources.push(format!("http://{}/note.txt", note.addr()));
    let ids = engine
        .add(sources, opts(dest.path(), true, false))
        .await
        .unwrap();
    assert_eq!(ids.len(), 2, "one archive package, one for the rest");

    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let snapshot = engine.snapshot().await;
            if snapshot.packages.len() == 2
                && snapshot
                    .packages
                    .iter()
                    .all(|p| p.status == PackageStatus::Done)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("never finished");
    let snapshot = engine.snapshot().await;
    let archive = snapshot
        .packages
        .iter()
        .find(|p| p.archive.is_some())
        .unwrap();
    assert!(archive.extract);
    let note_pkg = snapshot
        .packages
        .iter()
        .find(|p| p.archive.is_none())
        .unwrap();
    assert!(!note_pkg.extract);
    assert_extracted(dest.path());
    assert_eq!(
        std::fs::read(dest.path().join("note.txt")).unwrap(),
        b"just a note"
    );
    engine.shutdown().await;
}

#[tokio::test]
async fn a_restart_during_post_processing_finishes_the_job() {
    // Simulate a crash after download: a package left `Extracting` in its
    // manifest is verified and extracted again on the next start.
    let origins = serve_volumes("set.7z", &make_7z(&payload()), 2).await;
    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();
    {
        let engine = engine(data.path()).await;
        let drafts = engine
            .collect(origins.iter().map(Origin::url).collect())
            .await;
        engine
            .enqueue(
                drafts.into_iter().next().unwrap(),
                opts(dest.path(), true, false),
            )
            .await
            .unwrap();
        wait_status(&engine, |s| *s == PackageStatus::Done).await;
        engine.shutdown().await;
    }
    // Rewrite the manifest as if the process died while extracting, and
    // remove the extracted files.
    let packages_dir = data.path().join("packages");
    let manifest_path = std::fs::read_dir(&packages_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .unwrap();
    let text = std::fs::read_to_string(&manifest_path).unwrap();
    assert!(text.contains("\"Done\""), "{text}");
    std::fs::write(&manifest_path, text.replace("\"Done\"", "\"Extracting\"")).unwrap();
    std::fs::remove_file(dest.path().join("readme.txt")).unwrap();
    std::fs::remove_dir_all(dest.path().join("data")).unwrap();

    let engine = engine(data.path()).await;
    wait_status(&engine, |s| *s == PackageStatus::Done).await;
    assert_extracted(dest.path());
    engine.shutdown().await;
}
