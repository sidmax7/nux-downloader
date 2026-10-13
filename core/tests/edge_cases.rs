//! Things that go wrong on real machines (`IMPLEMENTATION_PLAN.md` P8.3):
//! the destination vanishes, a directory is not writable, paths that are
//! not UTF-8, servers that lie about a file's length, and files bigger
//! than 100 GiB. (A full disk has its own test in `cli/tests/disk_full.rs`.)

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nuxd_core::config::EngineConfig;
use nuxd_core::engine::sim::one_link;
use nuxd_core::model::{FileStatus, PackageError, PackageStatus, Priority};
use nuxd_core::net::bind::BindPolicy;
use nuxd_core::net::monitor::LinkSetup;
use nuxd_core::{AddOptions, Engine, Target};
use nuxd_testkit::{Fault, Origin, OriginOptions, content_range};
use std::path::Path;
use std::time::Duration;
use tempfile::tempdir;

async fn engine(data: &Path, config: EngineConfig) -> Engine {
    Engine::start_with(
        config,
        Some(data.to_path_buf()),
        LinkSetup::Fixed {
            links: vec![one_link()],
            bind: BindPolicy::Off,
        },
    )
    .await
    .unwrap()
}

fn opts(dest: &Path) -> AddOptions {
    AddOptions {
        name: "p".to_string(),
        dest_dir: dest.to_path_buf(),
        priority: Priority::Normal,
        extract: false,
        delete_archives: false,
        own_folder: false,
    }
}

async fn wait_for(engine: &Engine, what: &str, want: impl Fn(&nuxd_core::model::Package) -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let snapshot = engine.snapshot().await;
            if snapshot.packages.first().is_some_and(&want) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for: {what}"));
}

fn only_file(dir: &Path) -> std::path::PathBuf {
    let names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    assert_eq!(names.len(), 1, "{names:?}");
    names[0].clone()
}

#[tokio::test]
async fn a_destination_deleted_mid_download_fails_loudly_and_the_file_can_be_restarted() {
    let size = 2 * 1024 * 1024;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 41,
        ..Default::default()
    })
    .await;
    origin.set_fault(2, Fault::Throttle(1024 * 1024));
    let data = tempdir().unwrap();
    let outer = tempdir().unwrap();
    let dest = outer.path().join("dest");
    std::fs::create_dir(&dest).unwrap();
    let engine = engine(data.path(), EngineConfig::default()).await;
    engine.add(vec![origin.url()], opts(&dest)).await.unwrap();

    tokio::time::sleep(Duration::from_millis(600)).await;
    std::fs::remove_dir_all(&dest).unwrap();

    // The bytes had nowhere to go: the package must not claim success.
    wait_for(&engine, "the package to stop", |p| {
        matches!(p.status, PackageStatus::Failed(_) | PackageStatus::Paused)
    })
    .await;
    let snapshot = engine.snapshot().await;
    let package = &snapshot.packages[0];
    assert!(
        matches!(
            package.status,
            PackageStatus::Failed(PackageError::VerifyFailed { .. })
        ) || (package.status == PackageStatus::Paused && package.last_error.is_some()),
        "{:?} / {:?}",
        package.status,
        package.last_error
    );
    assert!(!dest.join("file").exists());

    // Put the directory back and start the file over: byte-exact.
    std::fs::create_dir(&dest).unwrap();
    match package.status {
        PackageStatus::Paused => {
            engine.resume(Target::All).await.unwrap();
        }
        _ => {
            let file = package
                .files
                .iter()
                .find(|f| matches!(f.status, FileStatus::Failed(_)))
                .expect("the lost file is marked failed");
            engine.restart_file(file.id).await.unwrap();
            engine.resume(Target::All).await.unwrap();
        }
    }
    wait_for(&engine, "the package to finish", |p| {
        p.status == PackageStatus::Done
    })
    .await;
    assert_eq!(
        std::fs::read(only_file(&dest)).unwrap(),
        content_range(41, 0, size)
    );
    engine.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn permission_denied_pauses_with_the_reason_and_resumes_after_it_is_fixed() {
    use std::os::unix::fs::PermissionsExt;
    let origin = Origin::start(OriginOptions {
        size: 50_000,
        seed: 42,
        ..Default::default()
    })
    .await;
    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();
    std::fs::set_permissions(dest.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    // (As root the permission check wouldn't bite; skip rather than lie.)
    if std::fs::File::create(dest.path().join("probe")).is_ok() {
        eprintln!("skipped: permissions are not enforced for this user");
        return;
    }
    let engine = engine(data.path(), EngineConfig::default()).await;
    engine
        .add(vec![origin.url()], opts(dest.path()))
        .await
        .unwrap();
    wait_for(&engine, "a pause on the error", |p| {
        p.status == PackageStatus::Paused && p.last_error.is_some()
    })
    .await;
    let snapshot = engine.snapshot().await;
    let reason = snapshot.packages[0]
        .last_error
        .as_ref()
        .unwrap()
        .to_string();
    assert!(reason.to_lowercase().contains("permission"), "{reason}");

    std::fs::set_permissions(dest.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    engine.resume(Target::All).await.unwrap();
    wait_for(&engine, "completion", |p| p.status == PackageStatus::Done).await;
    assert_eq!(
        std::fs::read(only_file(dest.path())).unwrap(),
        content_range(42, 0, 50_000)
    );
    engine.shutdown().await;
}

// macOS (APFS) refuses names that aren't valid UTF-8 ("Illegal byte
// sequence"), so such a folder can't exist there; Linux allows any bytes.
#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test]
async fn a_destination_that_is_not_utf8_works_and_survives_a_restart() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let origin = Origin::start(OriginOptions {
        size: 30_000,
        seed: 43,
        ..Default::default()
    })
    .await;
    origin.set_fault(2, Fault::StallBody);
    let data = tempdir().unwrap();
    let outer = tempdir().unwrap();
    let dest = outer.path().join(OsStr::from_bytes(b"caf\xE9-\xFF"));
    std::fs::create_dir(&dest).unwrap();

    {
        let engine = engine(data.path(), EngineConfig::default()).await;
        engine.add(vec![origin.url()], opts(&dest)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        engine.shutdown().await;
    }
    // The manifest must have been written and must read back the same path.
    let engine = engine(data.path(), EngineConfig::default()).await;
    let snapshot = engine.snapshot().await;
    assert_eq!(snapshot.packages.len(), 1, "the queue was lost on restart");
    assert_eq!(snapshot.packages[0].dest_dir, dest);
    wait_for(&engine, "completion", |p| p.status == PackageStatus::Done).await;
    assert_eq!(
        std::fs::read(only_file(&dest)).unwrap(),
        content_range(43, 0, 30_000)
    );
    engine.shutdown().await;
}

#[tokio::test]
async fn a_server_that_ends_every_response_early_fails_the_file_instead_of_hanging() {
    let origin = Origin::start(OriginOptions {
        size: 100_000,
        seed: 44,
        ..Default::default()
    })
    .await;
    // The probe (#1) is honest: it asks for one byte. Every transfer is
    // cut short, as a server lying about the length does.
    origin.set_default_fault(Fault::EndAfter(1_000));
    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let config = EngineConfig {
        retry_base_backoff: Duration::from_millis(20),
        retry_max_backoff: Duration::from_millis(50),
        max_retries_per_block: 3,
        ..Default::default()
    };
    let engine = engine(data.path(), config).await;
    engine
        .add(vec![origin.url()], opts(dest.path()))
        .await
        .unwrap();
    wait_for(&engine, "the file to fail", |p| {
        matches!(p.status, PackageStatus::Failed(_))
    })
    .await;
    let snapshot = engine.snapshot().await;
    assert!(matches!(
        snapshot.packages[0].files[0].status,
        FileStatus::Failed(_)
    ));
    engine.shutdown().await;
}

#[tokio::test]
async fn a_file_over_100_gib_is_planned_preallocated_sparse_and_resumable() {
    const SIZE: u64 = 120 * 1024 * 1024 * 1024;
    let origin = Origin::start(OriginOptions {
        size: SIZE,
        seed: 45,
        ..Default::default()
    })
    .await;
    // Trickle it: we only want the start of the transfer.
    origin.set_default_fault(Fault::Throttle(2 * 1024 * 1024));
    let scratch = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let data = scratch.path().join("data");
    let dest = scratch.path().join("dest");
    std::fs::create_dir_all(&dest).unwrap();
    let free = fs4::available_space(&dest).unwrap();

    let engine = engine(&data, EngineConfig::default()).await;
    engine.add(vec![origin.url()], opts(&dest)).await.unwrap();

    if free < SIZE + (1 << 30) {
        // Not enough room for 120 GiB: refusing up front, with the reason,
        // is the right answer.
        wait_for(&engine, "the space check", |p| {
            p.status == PackageStatus::Paused && p.last_error.is_some()
        })
        .await;
        let snapshot = engine.snapshot().await;
        let reason = snapshot.packages[0]
            .last_error
            .as_ref()
            .unwrap()
            .to_string();
        assert!(reason.contains("not enough free space"), "{reason}");
        engine.shutdown().await;
        return;
    }

    wait_for(&engine, "some progress", |p| {
        p.files[0]
            .parts
            .iter()
            .map(|part| part.durable)
            .sum::<u64>()
            > 0
    })
    .await;
    let snapshot = engine.snapshot().await;
    let file = &snapshot.packages[0].files[0];
    let layout = file.layout.unwrap();
    assert!(layout.count <= 4096, "{layout:?}");
    assert!(layout.block_size * u64::from(layout.count) >= SIZE);
    assert_eq!(file.parts.len(), layout.count as usize);

    // The placeholder is the full length but takes (almost) no space.
    let part = std::fs::read_dir(&dest)
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| e.path().extension().and_then(|x| x.to_str()) == Some("nuxdpart"))
        .expect("a .nuxdpart");
    let meta = part.metadata().unwrap();
    assert_eq!(meta.len(), SIZE);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert!(
            meta.blocks() * 512 < 1 << 30,
            "not sparse: {} blocks",
            meta.blocks()
        );
    }
    engine.shutdown().await;

    // And it resumes (the manifest with ~4k parts reads back).
    let engine = self::engine(&data, EngineConfig::default()).await;
    let snapshot = engine.snapshot().await;
    assert_eq!(snapshot.packages[0].files[0].layout, Some(layout));
    engine.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_planted_where_the_partial_file_goes_is_never_written_through() {
    let origin = Origin::start(OriginOptions {
        size: 40_000,
        seed: 46,
        ..Default::default()
    })
    .await;
    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let victim_dir = tempdir().unwrap();
    let victim = victim_dir.path().join("precious.txt");
    std::fs::write(&victim, b"do not touch").unwrap();
    // The download will be called `file`; plant its partial name.
    std::os::unix::fs::symlink(&victim, dest.path().join("file.nuxdpart")).unwrap();

    let engine = engine(data.path(), EngineConfig::default()).await;
    engine
        .add(vec![origin.url()], opts(dest.path()))
        .await
        .unwrap();
    wait_for(&engine, "the refusal", |p| {
        p.status == PackageStatus::Paused && p.last_error.is_some()
    })
    .await;
    assert_eq!(std::fs::read(&victim).unwrap(), b"do not touch");
    let snapshot = engine.snapshot().await;
    let reason = snapshot.packages[0]
        .last_error
        .as_ref()
        .unwrap()
        .to_string();
    assert!(
        reason.contains("symlink") || reason.contains("link"),
        "{reason}"
    );

    // The refusal cleared the link itself (never its target), so it
    // downloads normally on resume.
    assert!(!dest.path().join("file.nuxdpart").exists());
    engine.resume(Target::All).await.unwrap();
    wait_for(&engine, "completion", |p| p.status == PackageStatus::Done).await;
    assert_eq!(
        std::fs::read(dest.path().join("file")).unwrap(),
        content_range(46, 0, 40_000)
    );
    assert_eq!(std::fs::read(&victim).unwrap(), b"do not touch");
    engine.shutdown().await;
}

/// A destination far past Windows' old 260-character `MAX_PATH`. Rust's
/// standard library turns long paths into `\\?\` form on Windows, so this
/// must work on every platform (CI runs it on all three).
#[tokio::test]
async fn a_destination_path_longer_than_260_characters_works() {
    let size = 3 * 1024 * 1024;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 43,
        ..Default::default()
    })
    .await;
    let data = tempdir().unwrap();
    let outer = tempdir().unwrap();
    let mut dest = outer.path().to_path_buf();
    for level in 0..6 {
        dest.push(format!("{level}-{}", "deep-folder-name".repeat(4)));
    }
    assert!(dest.as_os_str().len() > 300, "{}", dest.display());
    std::fs::create_dir_all(&dest).unwrap();

    let engine = engine(data.path(), EngineConfig::default()).await;
    engine.add(vec![origin.url()], opts(&dest)).await.unwrap();
    wait_for(&engine, "the package to finish", |p| {
        p.status == PackageStatus::Done
    })
    .await;
    assert_eq!(
        std::fs::read(only_file(&dest)).unwrap(),
        content_range(43, 0, size)
    );
    engine.shutdown().await;
}
