//! Page scanning end to end (`ARCHITECTURE.md` §13.1): a pasted link that
//! is a web page is replaced by the files on it; a deep scan also opens
//! same-site pages one level down; a page that refuses is reported.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nuxd_core::collect::{CollectOptions, PackageDraft, collect_with};
use nuxd_core::config::EngineConfig;
use nuxd_core::net::client::build_default_client;
use nuxd_core::resolve::Registry;
use nuxd_testkit::{Fault, Origin, OriginOptions};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

async fn page(html: String) -> Origin {
    Origin::start(OriginOptions {
        body: Some(Arc::new(html.into_bytes())),
        extra_headers: vec![("Content-Type".into(), "text/html; charset=utf-8".into())],
        ..Default::default()
    })
    .await
}

async fn files() -> Origin {
    Origin::start(OriginOptions {
        size: 50_000,
        seed: 71,
        ..Default::default()
    })
    .await
}

/// Where `origin` serves `path` (every path serves the same content).
fn at(origin: &Origin, path: &str) -> String {
    format!("{}{path}", origin.url().trim_end_matches("/file"))
}

async fn collect(input: String, options: CollectOptions) -> Vec<PackageDraft> {
    let config = EngineConfig::default();
    let client = build_default_client(&config).unwrap();
    collect_with(
        vec![input],
        options,
        &config,
        &Registry::default(),
        &client,
        &CancellationToken::new(),
    )
    .await
}

fn names(drafts: &[PackageDraft]) -> Vec<String> {
    let mut names: Vec<String> = drafts
        .iter()
        .flat_map(|d| &d.files)
        .map(|f| {
            f.probe
                .as_ref()
                .map(|p| p.suggested_name.clone())
                .unwrap_or_default()
        })
        .collect();
    names.sort();
    names
}

const SCAN: CollectOptions = CollectOptions {
    scan_pages: true,
    deep: false,
};
const DEEP: CollectOptions = CollectOptions {
    scan_pages: true,
    deep: true,
};

#[tokio::test]
async fn a_page_is_replaced_by_the_files_it_links_to() {
    let cdn = files().await;
    let album = page(format!(
        r#"<html><head><title>Summer  Album</title></head><body>
           <a href="{one}">one</a> <img src="{two}"> <a href="/about">About us</a>
           <a href="{one}">one again</a></body></html>"#,
        one = at(&cdn, "/media/one.zip"),
        two = at(&cdn, "/media/two.jpg"),
    ))
    .await;

    let drafts = collect(album.url(), SCAN).await;
    assert_eq!(names(&drafts), ["one.zip", "two.jpg"], "{drafts:?}");
    // The zip gets its own package (extraction is offered for it); the
    // rest is named after the page.
    assert!(
        drafts.iter().any(|d| d.name == "Summer Album"),
        "{drafts:?}"
    );
    let all: Vec<_> = drafts.iter().flat_map(|d| &d.files).collect();
    assert!(all.iter().all(|f| f.probe.is_ok()));
    assert!(
        all.iter()
            .all(|f| f.found_on.as_deref() == Some(album.url().as_str()))
    );

    // Without scanning, the page itself is what's offered.
    let plain = collect(album.url(), CollectOptions::default()).await;
    assert_eq!(plain.iter().map(|d| d.files.len()).sum::<usize>(), 1);
}

#[tokio::test]
async fn a_deep_scan_opens_same_site_pages_one_level_down() {
    let cdn = files().await;
    // The album links to a file page; only the file page links the file.
    let file_page = page(format!(
        r#"<title>three</title><a href="{three}">Download</a>"#,
        three = at(&cdn, "/get/three.mkv"),
    ))
    .await;
    let album = page(format!(
        r#"<title>Deep Album</title><a href="{f}">three</a>"#,
        f = at(&file_page, "/f/abc123"),
    ))
    .await;

    let shallow = collect(album.url(), SCAN).await;
    assert!(names(&shallow).is_empty(), "{shallow:?}");

    let deep = collect(album.url(), DEEP).await;
    assert_eq!(names(&deep), ["three.mkv"], "{deep:?}");
    assert_eq!(deep[0].name, "Deep Album");
}

#[tokio::test]
async fn a_page_that_refuses_is_reported_not_worked_around() {
    // Request 1 is the probe (fine); request 2 fetches the page.
    let guarded = page("<title>x</title>".to_string()).await;
    guarded.set_fault(2, Fault::Status(403));

    let drafts = collect(guarded.url(), SCAN).await;
    let errors: Vec<String> = drafts
        .iter()
        .flat_map(|d| &d.files)
        .filter_map(|f| f.probe.as_ref().err().cloned())
        .collect();
    assert_eq!(errors.len(), 1, "{drafts:?}");
    assert!(errors[0].contains("refused"), "{}", errors[0]);
    // One probe and one page request: nothing retried, nothing else tried.
    assert_eq!(guarded.requests().len(), 2);
}

#[tokio::test]
async fn a_deep_scan_opens_pages_in_parallel_with_one_request_each() {
    let cdn = files().await;
    // Every item page links the same video; each takes 150 ms to answer.
    let items = page(format!(
        r#"<title>item</title><a href="{v}">Download</a>"#,
        v = at(&cdn, "/get/clip.mp4"),
    ))
    .await;
    items.set_default_fault(Fault::Delay(150));
    let links: String = (1..=24)
        .map(|n| {
            format!(
                r#"<a href="{}">item {n}</a>"#,
                at(&items, &format!("/item/{n}"))
            )
        })
        .collect();
    let album = page(format!("<title>Big Album</title>{links}")).await;

    let started = std::time::Instant::now();
    let drafts = collect(album.url(), DEEP).await;
    let took = started.elapsed();

    assert_eq!(names(&drafts), ["clip.mp4"], "{drafts:?}");
    // 24 pages × 150 ms is 3.6 s one by one; six at a time is about 0.6 s.
    assert!(took < std::time::Duration::from_millis(2_000), "{took:?}");
    // One GET per page: no probe of a page before opening it.
    let requests = items.requests();
    assert_eq!(requests.len(), 24, "{} requests", requests.len());
    assert!(
        requests.iter().all(|r| r.range.is_none()),
        "a page was probed first"
    );
}
