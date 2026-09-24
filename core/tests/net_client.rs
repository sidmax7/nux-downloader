//! `net::client` against the testkit origin (`IMPLEMENTATION_PLAN.md`
//! P2.3's done-when): HTTP/1.1, `Accept-Encoding: identity`, and no
//! transparent decompression of a gzip-labelled body.

use nuxd_core::config::EngineConfig;
use nuxd_core::net::client::{build_default_client, build_link_client};
use nuxd_testkit::{Origin, OriginOptions, content_range};

#[tokio::test]
async fn default_client_speaks_http1_1() {
    let origin = Origin::start(OriginOptions {
        size: 100,
        seed: 1,
        ..Default::default()
    })
    .await;
    let client = build_default_client(&EngineConfig::default()).unwrap();

    let response = client.get(origin.url()).send().await.unwrap();
    assert_eq!(response.version(), reqwest::Version::HTTP_11);
}

#[tokio::test]
async fn link_client_speaks_http1_1() {
    let origin = Origin::start(OriginOptions {
        size: 100,
        seed: 2,
        ..Default::default()
    })
    .await;
    let client = build_link_client(&EngineConfig::default()).unwrap();

    let response = client.get(origin.url()).send().await.unwrap();
    assert_eq!(response.version(), reqwest::Version::HTTP_11);
}

#[tokio::test]
async fn sends_accept_encoding_identity() {
    let origin = Origin::start(OriginOptions {
        size: 10,
        seed: 3,
        ..Default::default()
    })
    .await;
    let client = build_default_client(&EngineConfig::default()).unwrap();

    let _ = client.get(origin.url()).send().await.unwrap();
    let requests = origin.requests();
    assert_eq!(requests[0].accept_encoding.as_deref(), Some("identity"));
}

#[tokio::test]
async fn does_not_decompress_a_gzip_labelled_body() {
    // The origin serves *raw* (not actually gzip-compressed) bytes but
    // labels them `Content-Encoding: gzip`. A client that transparently
    // decompressed would either error trying to inflate non-gzip data,
    // or (if somehow tolerant) hand back something other than the raw
    // bytes. Getting back exactly the raw bytes proves the label was
    // never acted on — which is guaranteed by construction here, since
    // `reqwest` is built without decompression features at all (see
    // `core/Cargo.toml`), but this exercises the whole path end to end
    // rather than just trusting the Cargo feature flags.
    let origin = Origin::start(OriginOptions {
        size: 256,
        seed: 4,
        extra_headers: vec![("content-encoding".to_string(), "gzip".to_string())],
        ..Default::default()
    })
    .await;
    let client = build_default_client(&EngineConfig::default()).unwrap();

    let response = client.get(origin.url()).send().await.unwrap();
    let body = response.bytes().await.unwrap();
    assert_eq!(body.as_ref(), content_range(4, 0, 256).as_slice());
}

#[tokio::test]
async fn follows_redirects_up_to_the_limit() {
    let origin = Origin::start(OriginOptions {
        size: 10,
        seed: 5,
        ..Default::default()
    })
    .await;
    // Only the first request redirects — request #2 (the followed hop)
    // gets the default `Ok` fault and serves normally.
    origin.set_fault(1, nuxd_testkit::Fault::Redirect(origin.url()));
    let client = build_default_client(&EngineConfig::default()).unwrap();

    let response = client.get(origin.url()).send().await.unwrap();
    // The first hop redirects to the file's real URL, which the second
    // request serves normally (no fault set for request #2).
    assert_eq!(response.status(), 200);
}
