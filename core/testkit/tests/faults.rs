//! One test per fault (`IMPLEMENTATION_PLAN.md` P2.1's done-when), plus
//! the request log and the hold gate. Uses `reqwest` as a plain HTTP
//! client — this crate must not depend on `nuxd-core`, so it can't use
//! anything from there to talk to itself.

use bytes::Bytes;
use nuxd_testkit::{Fault, Origin, OriginOptions, content_range};
use reqwest::header::RANGE;
use std::time::Duration;

async fn origin(options: OriginOptions) -> Origin {
    Origin::start(options).await
}

#[tokio::test]
async fn serves_the_whole_file_on_a_plain_get() {
    let origin = origin(OriginOptions {
        size: 256,
        seed: 1,
        ..Default::default()
    })
    .await;
    let response = reqwest::get(origin.url()).await.unwrap();
    assert_eq!(response.status(), 200);
    let body = response.bytes().await.unwrap();
    assert_eq!(body.as_ref(), content_range(1, 0, 256).as_slice());
}

#[tokio::test]
async fn serves_a_requested_byte_range() {
    let origin = origin(OriginOptions {
        size: 1000,
        seed: 2,
        ..Default::default()
    })
    .await;
    let client = reqwest::Client::new();
    let response = client
        .get(origin.url())
        .header(RANGE, "bytes=100-199")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 206);
    let content_range_header = response
        .headers()
        .get("content-range")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(content_range_header, "bytes 100-199/1000");
    let body = response.bytes().await.unwrap();
    assert_eq!(body.as_ref(), content_range(2, 100, 100).as_slice());
}

#[tokio::test]
async fn open_ended_range_serves_to_the_end() {
    let origin = origin(OriginOptions {
        size: 500,
        seed: 3,
        ..Default::default()
    })
    .await;
    let client = reqwest::Client::new();
    let response = client
        .get(origin.url())
        .header(RANGE, "bytes=490-")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 206);
    let body = response.bytes().await.unwrap();
    assert_eq!(body.len(), 10);
}

#[tokio::test]
async fn range_at_or_past_the_end_is_416_with_bytes_star_total() {
    let origin = origin(OriginOptions {
        size: 100,
        seed: 4,
        ..Default::default()
    })
    .await;
    let client = reqwest::Client::new();
    let response = client
        .get(origin.url())
        .header(RANGE, "bytes=100-100")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 416);
    let content_range_header = response
        .headers()
        .get("content-range")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(content_range_header, "bytes */100");
}

#[tokio::test]
async fn an_empty_file_answers_byte_zero_with_416_bytes_star_zero() {
    // The specific case ARCHITECTURE.md §5 calls out: a 416 on `bytes=0-0`
    // with `bytes */0` is a valid empty download, not an error.
    let origin = origin(OriginOptions {
        size: 0,
        seed: 5,
        ..Default::default()
    })
    .await;
    let client = reqwest::Client::new();
    let response = client
        .get(origin.url())
        .header(RANGE, "bytes=0-0")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 416);
    let content_range_header = response
        .headers()
        .get("content-range")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(content_range_header, "bytes */0");
}

#[tokio::test]
async fn ignore_range_serves_the_whole_file_despite_a_range_header() {
    let origin = origin(OriginOptions {
        size: 300,
        seed: 6,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::IgnoreRange);
    let client = reqwest::Client::new();
    let response = client
        .get(origin.url())
        .header(RANGE, "bytes=10-19")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = response.bytes().await.unwrap();
    assert_eq!(body.len(), 300);
}

#[tokio::test]
async fn wrong_start_lies_about_where_the_range_begins() {
    let origin = origin(OriginOptions {
        size: 300,
        seed: 7,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::WrongStart);
    let client = reqwest::Client::new();
    let response = client
        .get(origin.url())
        .header(RANGE, "bytes=50-99")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 206);
    let content_range_header = response
        .headers()
        .get("content-range")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(content_range_header, "bytes 51-99/300");
}

#[tokio::test]
async fn overlong_sends_one_byte_more_than_the_range_promised() {
    let origin = origin(OriginOptions {
        size: 300,
        seed: 8,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::Overlong);
    let client = reqwest::Client::new();
    let response = client
        .get(origin.url())
        .header(RANGE, "bytes=0-9")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 206);
    let content_range_header = response
        .headers()
        .get("content-range")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(content_range_header, "bytes 0-9/300"); // header still claims 10 bytes
    let body = response.bytes().await.unwrap();
    assert_eq!(body.len(), 11); // but 11 were actually sent
}

#[tokio::test]
async fn no_content_range_omits_the_header_on_a_206() {
    let origin = origin(OriginOptions {
        size: 300,
        seed: 9,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::NoContentRange);
    let client = reqwest::Client::new();
    let response = client
        .get(origin.url())
        .header(RANGE, "bytes=0-9")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 206);
    assert!(response.headers().get("content-range").is_none());
}

#[tokio::test]
async fn stall_headers_never_answers() {
    let origin = origin(OriginOptions {
        size: 300,
        seed: 10,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::StallHeaders);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(300))
        .build()
        .unwrap();
    let result = client.get(origin.url()).send().await;
    assert!(
        result.is_err(),
        "expected the request to time out, got {result:?}"
    );
}

#[tokio::test]
async fn stall_body_sends_headers_then_nothing() {
    let origin = origin(OriginOptions {
        size: 300,
        seed: 11,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::StallBody);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(300))
        .build()
        .unwrap();
    let response = client.get(origin.url()).send().await.unwrap();
    assert_eq!(response.status(), 200); // headers arrived...
    let body_result = response.bytes().await;
    assert!(body_result.is_err(), "expected the body read to time out"); // ...but the body never does
}

#[tokio::test]
async fn cut_after_ends_the_connection_abruptly() {
    let origin = origin(OriginOptions {
        size: 10_000,
        seed: 12,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::CutAfter(500));
    // Depending on exactly when the abrupt close lands relative to
    // hyper's own buffering, it can surface either while awaiting the
    // response itself or while reading its body — the property under
    // test is that the transfer as a whole fails, not which call it
    // fails on.
    let result = async {
        let response = reqwest::get(origin.url()).await?;
        response.bytes().await
    }
    .await;
    assert!(
        result.is_err(),
        "expected the transfer to fail, got {result:?}"
    );
}

#[tokio::test]
async fn end_after_ends_the_body_cleanly_but_short() {
    let origin = origin(OriginOptions {
        size: 10_000,
        seed: 13,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::EndAfter(500));
    let response = reqwest::get(origin.url()).await.unwrap();
    let body = response.bytes().await.unwrap(); // ends cleanly...
    assert_eq!(body.len(), 500); // ...but short of the full 10,000
    assert_eq!(body.as_ref(), content_range(13, 0, 500).as_slice());
}

#[tokio::test]
async fn crawl_delivers_the_full_body_slowly() {
    let origin = origin(OriginOptions {
        size: 200,
        seed: 14,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::Crawl(2000)); // ~200ms for 200 bytes at this rate
    let started = std::time::Instant::now();
    let response = reqwest::get(origin.url()).await.unwrap();
    let body = response.bytes().await.unwrap();
    assert_eq!(body.as_ref(), content_range(14, 0, 200).as_slice());
    assert!(
        started.elapsed() >= Duration::from_millis(50),
        "expected a trickled response"
    );
}

#[tokio::test]
async fn status_fault_replies_with_that_status_and_no_body() {
    let origin = origin(OriginOptions {
        size: 300,
        seed: 15,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::Status(503));
    let response = reqwest::get(origin.url()).await.unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(response.bytes().await.unwrap().len(), 0);
}

#[tokio::test]
async fn redirect_fault_sends_a_302_with_location() {
    let origin = origin(OriginOptions {
        size: 300,
        seed: 16,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::Redirect("https://example.com/elsewhere".to_string()));
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let response = client.get(origin.url()).send().await.unwrap();
    assert_eq!(response.status(), 302);
    assert_eq!(
        response.headers().get("location").unwrap(),
        "https://example.com/elsewhere"
    );
}

#[tokio::test]
async fn no_range_support_ignores_the_range_header_on_every_request() {
    let origin = origin(OriginOptions {
        size: 300,
        seed: 17,
        ranges: false,
        ..Default::default()
    })
    .await;
    let client = reqwest::Client::new();
    let response = client
        .get(origin.url())
        .header(RANGE, "bytes=0-9")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().len(), 300);
}

#[tokio::test]
async fn unknown_size_omits_content_length() {
    let origin = origin(OriginOptions {
        size: 300,
        seed: 18,
        content_length: false,
        ..Default::default()
    })
    .await;
    let response = reqwest::get(origin.url()).await.unwrap();
    assert!(response.headers().get("content-length").is_none());
    assert_eq!(response.bytes().await.unwrap().len(), 300);
}

#[tokio::test]
async fn etag_and_last_modified_are_served_when_set() {
    let origin = origin(OriginOptions {
        size: 10,
        seed: 19,
        etag: Some("\"abc123\"".to_string()),
        last_modified: Some("Mon, 01 Jan 2024 00:00:00 GMT".to_string()),
        ..Default::default()
    })
    .await;
    let response = reqwest::get(origin.url()).await.unwrap();
    assert_eq!(response.headers().get("etag").unwrap(), "\"abc123\"");
    assert_eq!(
        response.headers().get("last-modified").unwrap(),
        "Mon, 01 Jan 2024 00:00:00 GMT"
    );
}

#[tokio::test]
async fn fault_for_a_specific_request_only_applies_once() {
    let origin = origin(OriginOptions {
        size: 100,
        seed: 20,
        ..Default::default()
    })
    .await;
    origin.set_fault(2, Fault::Status(500));

    let first = reqwest::get(origin.url()).await.unwrap();
    assert_eq!(first.status(), 200);
    let second = reqwest::get(origin.url()).await.unwrap();
    assert_eq!(second.status(), 500);
    let third = reqwest::get(origin.url()).await.unwrap();
    assert_eq!(third.status(), 200); // back to the default
}

#[tokio::test]
async fn request_log_records_range_and_outcome() {
    let origin = origin(OriginOptions {
        size: 100,
        seed: 21,
        ..Default::default()
    })
    .await;
    let client = reqwest::Client::new();
    let _ = client
        .get(origin.url())
        .header(RANGE, "bytes=5-14")
        .send()
        .await
        .unwrap();

    let requests = origin.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].n, 1);
    assert_eq!(requests[0].status, 206);
    assert_eq!(requests[0].range.unwrap().start, 5);
    assert_eq!(requests[0].range.unwrap().end, Some(14));
    assert_eq!(
        requests[0]
            .bytes_sent
            .load(std::sync::atomic::Ordering::Relaxed),
        10
    );
}

#[tokio::test]
async fn concurrency_from_one_source_is_tracked() {
    let origin = origin(OriginOptions {
        size: 1_000_000,
        seed: 22,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::Crawl(2000)); // slow enough to overlap requests
    let client = reqwest::Client::new();

    let a = client.get(origin.url()).send();
    let b = client.get(origin.url()).send();
    let (a, b) = tokio::join!(a, b);
    drop((a.unwrap(), b.unwrap()));

    let loopback = std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
    assert!(origin.peak_concurrent_from(loopback) >= 2);
}

#[tokio::test]
async fn hold_pauses_a_request_until_release() {
    let origin = origin(OriginOptions {
        size: 10_000,
        seed: 23,
        ..Default::default()
    })
    .await;
    origin.hold_next_request_at(100);

    let mut response = reqwest::get(origin.url()).await.unwrap();
    let mut received = Vec::<u8>::new();
    let read_100 = async {
        while received.len() < 100 {
            if let Some(chunk) = response.chunk().await.unwrap() {
                received.extend_from_slice(&chunk);
            } else {
                break;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(2), read_100)
        .await
        .expect("held request never sent its first 100 bytes");
    assert!(received.len() >= 100);

    // Nothing more should show up while held.
    let more = tokio::time::timeout(Duration::from_millis(200), response.chunk()).await;
    assert!(
        more.is_err(),
        "expected the held request to still be paused"
    );

    origin.release();
    let rest = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(chunk) = response.chunk().await.unwrap() {
            received.extend_from_slice(&chunk);
        }
    })
    .await;
    assert!(rest.is_ok(), "expected the request to finish after release");
    assert_eq!(
        Bytes::from(received).as_ref(),
        content_range(23, 0, 10_000).as_slice()
    );
}

#[tokio::test]
async fn shutdown_stops_the_server() {
    let origin = origin(OriginOptions {
        size: 10,
        seed: 24,
        ..Default::default()
    })
    .await;
    let url = origin.url();
    origin.shutdown().await;
    // Give the OS a moment to actually release the port / refuse new connections.
    // Either a connect error or a timeout means it's down — anything else
    // (a real response) is the failure case.
    let result = tokio::time::timeout(Duration::from_secs(1), reqwest::get(url)).await;
    if let Ok(Ok(response)) = result {
        panic!(
            "expected the server to be down, got status {}",
            response.status()
        );
    }
}

#[tokio::test]
async fn logs_the_accept_encoding_header_when_sent() {
    let origin = origin(OriginOptions {
        size: 10,
        seed: 25,
        ..Default::default()
    })
    .await;
    let client = reqwest::Client::new();
    let _ = client
        .get(origin.url())
        .header("accept-encoding", "identity")
        .send()
        .await
        .unwrap();
    let requests = origin.requests();
    assert_eq!(requests[0].accept_encoding.as_deref(), Some("identity"));
}

#[tokio::test]
async fn extra_headers_are_added_to_every_response() {
    let origin = origin(OriginOptions {
        size: 10,
        seed: 26,
        extra_headers: vec![("content-encoding".to_string(), "gzip".to_string())],
        ..Default::default()
    })
    .await;
    let response = reqwest::get(origin.url()).await.unwrap();
    assert_eq!(response.headers().get("content-encoding").unwrap(), "gzip");
}
