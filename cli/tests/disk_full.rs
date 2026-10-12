//! A full disk pauses the download with the reason, and `resume` finishes
//! it once there is room (`IMPLEMENTATION_PLAN.md` P8.3). The disk is a
//! 4 MiB tmpfs mounted inside a private user+mount namespace
//! (`unshare -rm`), so no privileges are needed; where that isn't allowed
//! the test says so and passes.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use nuxd_testkit::{Fault, Origin, OriginOptions, content_range};
use tempfile::tempdir;
use tokio::process::Command;

async fn can_unshare() -> bool {
    Command::new("unshare")
        .args(["-rm", "true"])
        .output()
        .await
        .is_ok_and(|o| o.status.success())
}

#[tokio::test]
async fn a_full_disk_pauses_the_package_and_resume_completes_it() {
    if !can_unshare().await {
        eprintln!("skipped: `unshare -rm` is not available here");
        return;
    }
    let size = 2 * 1024 * 1024;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 17,
        ..Default::default()
    })
    .await;
    // Slow enough that the filler lands mid-download; only for the first
    // transfer, so the resumed one is quick.
    origin.set_fault(2, Fault::Throttle(1024 * 1024));

    let work = tempdir().unwrap();
    let data = work.path().join("data");
    let mount = work.path().join("disk");
    std::fs::create_dir_all(&mount).unwrap();
    let expected = work.path().join("expected");
    std::fs::write(&expected, content_range(17, 0, size)).unwrap();

    let nuxd = env!("CARGO_BIN_EXE_nuxd");
    let script = format!(
        r#"
set -u
mount -t tmpfs -o size=4m tmpfs "{mount}" || exit 90
( sleep 0.6; dd if=/dev/zero of="{mount}/filler" bs=1k count=3900 2>/dev/null ) &
"{nuxd}" --data-dir "{data}" --unbound get "{url}" -o "{mount}" 2>&1
echo "first=$?"
"{nuxd}" --data-dir "{data}" --unbound queue 2>&1
wait
rm -f "{mount}/filler"
"{nuxd}" --data-dir "{data}" --unbound resume 2>&1
echo "second=$?"
cmp "{mount}/file" "{expected}" && echo same
"#,
        mount = mount.display(),
        nuxd = nuxd,
        data = data.display(),
        url = origin.url(),
        expected = expected.display(),
    );
    let output = Command::new("unshare")
        .args(["-rm", "sh", "-c", &script])
        .output()
        .await
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if output.status.code() == Some(90) {
        eprintln!("skipped: mounting a tmpfs isn't allowed here\n{text}");
        return;
    }
    assert!(
        text.contains("first=1"),
        "first run should stop on the error:\n{text}"
    );
    assert!(text.contains("is paused"), "{text}");
    assert!(
        text.to_lowercase().contains("no space"),
        "the reason should be shown:\n{text}"
    );
    // The queue shows it too.
    assert!(text.contains("paused: "), "{text}");
    assert!(text.contains("second=0"), "resume should finish:\n{text}");
    assert!(
        text.contains("same"),
        "the file must be byte-exact:\n{text}"
    );
}
