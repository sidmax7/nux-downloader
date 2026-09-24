//! `nuxd links` and `nuxd get --links` (`IMPLEMENTATION_PLAN.md` P3.8).
//! What links exist depends on the machine, so these only check things
//! that hold on any machine.

use tokio::process::Command;

fn nuxd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_nuxd"))
}

#[tokio::test]
async fn links_lists_or_explains_and_succeeds() {
    let output = nuxd()
        .arg("links")
        .output()
        .await
        .unwrap_or_else(|error| panic!("couldn't run nuxd: {error}"));
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("NAME") || text.contains("no usable network links"),
        "unexpected output: {text}"
    );
}

#[tokio::test]
async fn get_with_only_unknown_links_is_refused_up_front() {
    let data_dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let output = nuxd()
        .args(["--data-dir"])
        .arg(data_dir.path())
        .args(["get", "http://127.0.0.1:9/file", "--links", "nosuchlink0"])
        .output()
        .await
        .unwrap_or_else(|error| panic!("couldn't run nuxd: {error}"));
    assert_eq!(output.status.code(), Some(2));
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("nosuchlink0"), "unexpected stderr: {text}");
}
