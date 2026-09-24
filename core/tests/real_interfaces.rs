//! Real interface binding, on a machine that has one (P3.10's automatable
//! part). Ignored by default: it needs at least two up, non-loopback
//! interfaces, and `SO_BINDTODEVICE` may need privileges on some
//! systems. Run with `cargo test -p nuxd-core --test real_interfaces --
//! --ignored --nocapture`.

use nuxd_core::config::EngineConfig;
use nuxd_core::net::bind::{BindPolicy, build_bound_client};
use nuxd_core::net::discover::{SystemLinkSource, discover};
use nuxd_testkit::{Origin, OriginOptions};
use std::net::{IpAddr, SocketAddr};

/// Each real link's own client reaches an origin listening on the *other*
/// links' addresses only if the OS lets device-bound sockets use the
/// local table; what this checks is the reliable half: an origin bound to
/// a link's own address sees a request from that link's address.
#[tokio::test]
#[ignore = "needs real network interfaces"]
async fn device_bound_clients_reach_an_origin_on_their_own_address() {
    let config = EngineConfig::default();
    let links = discover(&SystemLinkSource, &config)
        .unwrap_or_else(|error| panic!("discovery failed: {error}"));
    let candidates: Vec<_> = links.iter().filter(|l| l.v4.is_some()).collect();
    assert!(!candidates.is_empty(), "no usable link on this machine");

    for link in candidates {
        let ip = IpAddr::V4(link.v4.unwrap_or_else(|| panic!("has v4")));
        let origin = Origin::start(OriginOptions {
            bind_addr: SocketAddr::new(ip, 0),
            ..Default::default()
        })
        .await;
        let mode = BindPolicy::Auto
            .mode_for(link)
            .unwrap_or_else(|| panic!("no bind mode for {}", link.name));
        let client = build_bound_client(&config, link, &mode)
            .unwrap_or_else(|error| panic!("client for {}: {error}", link.name));
        let outcome = client.get(origin.url()).send().await;
        println!(
            "{} ({ip}) via {mode:?}: {:?}",
            link.name,
            outcome.as_ref().map(|r| r.status())
        );
        let response = outcome.unwrap_or_else(|error| panic!("{}: {error}", link.name));
        assert!(response.status().is_success());
        assert_eq!(origin.requests().len(), 1);
        origin.shutdown().await;
    }
}
