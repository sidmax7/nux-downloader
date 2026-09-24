//! `probe::probe` against the testkit origin (`IMPLEMENTATION_PLAN.md`
//! P2.4's done-when): redirects, `206`/`200` handling, `416 bytes */0` as
//! an empty file, and a stall timeout.

use nuxd_core::config::EngineConfig;
use nuxd_core::error::ProbeError;
use nuxd_core::net::client::build_default_client;
use nuxd_core::probe::probe;
use nuxd_testkit::{Fault, Origin, OriginOptions};
use std::time::Duration;
use url::Url;

fn fast_timeout_config() -> EngineConfig {
    EngineConfig {
        stall_timeout: Duration::from_millis(300),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_206_response_reports_size_and_range_support() {
    let origin = Origin::start(OriginOptions {
        size: 1000,
        seed: 1,
        ..Default::default()
    })
    .await;
    let client = build_default_client(&EngineConfig::default()).unwrap();

    let result = probe(
        &client,
        Url::parse(&origin.url()).unwrap(),
        &EngineConfig::default(),
    )
    .await
    .unwrap();

    assert_eq!(result.expected_size, Some(1000));
    assert!(result.supports_ranges);
    assert_eq!(result.version.total_bytes, 1000);
    assert_eq!(result.version.etag.as_deref(), Some("\"testkit-etag\""));
}

#[tokio::test]
async fn a_200_response_reports_size_but_no_range_support() {
    let origin = Origin::start(OriginOptions {
        size: 500,
        seed: 2,
        ranges: false,
        ..Default::default()
    })
    .await;
    let client = build_default_client(&EngineConfig::default()).unwrap();

    let result = probe(
        &client,
        Url::parse(&origin.url()).unwrap(),
        &EngineConfig::default(),
    )
    .await
    .unwrap();

    assert_eq!(result.expected_size, Some(500));
    assert!(!result.supports_ranges);
}

#[tokio::test]
async fn a_200_with_no_content_length_reports_unknown_size() {
    let origin = Origin::start(OriginOptions {
        size: 500,
        seed: 3,
        ranges: false,
        content_length: false,
        ..Default::default()
    })
    .await;
    let client = build_default_client(&EngineConfig::default()).unwrap();

    let result = probe(
        &client,
        Url::parse(&origin.url()).unwrap(),
        &EngineConfig::default(),
    )
    .await
    .unwrap();

    assert_eq!(result.expected_size, None);
    assert!(!result.supports_ranges);
}

#[tokio::test]
async fn a_416_with_bytes_star_0_is_an_empty_file() {
    let origin = Origin::start(OriginOptions {
        size: 0,
        seed: 4,
        ..Default::default()
    })
    .await;
    let client = build_default_client(&EngineConfig::default()).unwrap();

    let result = probe(
        &client,
        Url::parse(&origin.url()).unwrap(),
        &EngineConfig::default(),
    )
    .await
    .unwrap();

    assert_eq!(result.expected_size, Some(0));
    assert!(!result.supports_ranges);
    assert_eq!(result.version.total_bytes, 0);
}

#[tokio::test]
async fn a_redirect_is_followed_and_the_final_url_is_reported() {
    let origin = Origin::start(OriginOptions {
        size: 10,
        seed: 5,
        ..Default::default()
    })
    .await;
    origin.set_fault(1, Fault::Redirect(origin.url()));
    let client = build_default_client(&EngineConfig::default()).unwrap();

    let result = probe(
        &client,
        Url::parse(&origin.url()).unwrap(),
        &EngineConfig::default(),
    )
    .await
    .unwrap();

    assert_eq!(result.final_url, Url::parse(&origin.url()).unwrap());
    assert_eq!(result.expected_size, Some(10));
}

#[tokio::test]
async fn a_stalled_response_times_out() {
    let origin = Origin::start(OriginOptions {
        size: 10,
        seed: 6,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::StallHeaders);
    let client = build_default_client(&EngineConfig::default()).unwrap();

    let result = probe(
        &client,
        Url::parse(&origin.url()).unwrap(),
        &fast_timeout_config(),
    )
    .await;

    assert!(matches!(result, Err(ProbeError::TimedOut)));
}

#[tokio::test]
async fn a_206_with_no_content_range_is_malformed() {
    let origin = Origin::start(OriginOptions {
        size: 10,
        seed: 7,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::NoContentRange);
    let client = build_default_client(&EngineConfig::default()).unwrap();

    let result = probe(
        &client,
        Url::parse(&origin.url()).unwrap(),
        &EngineConfig::default(),
    )
    .await;

    assert!(matches!(result, Err(ProbeError::MalformedContentRange)));
}

#[tokio::test]
async fn a_206_starting_at_the_wrong_byte_is_malformed() {
    let origin = Origin::start(OriginOptions {
        size: 10,
        seed: 8,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::WrongStart);
    let client = build_default_client(&EngineConfig::default()).unwrap();

    let result = probe(
        &client,
        Url::parse(&origin.url()).unwrap(),
        &EngineConfig::default(),
    )
    .await;

    assert!(matches!(result, Err(ProbeError::MalformedContentRange)));
}

#[tokio::test]
async fn a_server_that_ignores_range_still_probes_successfully_as_unranged() {
    let origin = Origin::start(OriginOptions {
        size: 10,
        seed: 9,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::IgnoreRange);
    let client = build_default_client(&EngineConfig::default()).unwrap();

    let result = probe(
        &client,
        Url::parse(&origin.url()).unwrap(),
        &EngineConfig::default(),
    )
    .await
    .unwrap();

    assert_eq!(result.expected_size, Some(10));
    assert!(!result.supports_ranges);
}

#[tokio::test]
async fn a_404_is_an_unexpected_status() {
    let origin = Origin::start(OriginOptions {
        size: 10,
        seed: 10,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::Status(404));
    let client = build_default_client(&EngineConfig::default()).unwrap();

    let result = probe(
        &client,
        Url::parse(&origin.url()).unwrap(),
        &EngineConfig::default(),
    )
    .await;

    assert!(matches!(
        result,
        Err(ProbeError::UnexpectedStatus { status: 404 })
    ));
}

#[tokio::test]
async fn content_disposition_names_the_suggested_filename() {
    let origin = Origin::start(OriginOptions {
        size: 10,
        seed: 11,
        content_disposition: Some("attachment; filename=\"report.pdf\"".to_string()),
        ..Default::default()
    })
    .await;
    let client = build_default_client(&EngineConfig::default()).unwrap();

    let result = probe(
        &client,
        Url::parse(&origin.url()).unwrap(),
        &EngineConfig::default(),
    )
    .await
    .unwrap();

    assert_eq!(result.suggested_name, "report.pdf");
}
