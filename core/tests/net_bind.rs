//! `net::bind` against the testkit: a client bound to a loopback "link"
//! (`127.0.0.x`, `ARCHITECTURE.md` §15.2) really connects from that
//! source address — the property multi-link tests attribute traffic by.

use nuxd_core::config::EngineConfig;
use nuxd_core::model::{Link, LinkId, LinkKind};
use nuxd_core::net::bind::{BindMode, build_bound_client};
use nuxd_testkit::{Origin, OriginOptions};
use std::net::IpAddr;

fn loopback_link(ip: &str) -> Link {
    Link {
        id: LinkId::new(1),
        name: format!("lo-{ip}"),
        label: ip.to_string(),
        kind: LinkKind::Other,
        v4: Some(
            ip.parse()
                .unwrap_or_else(|error| panic!("bad ip {ip}: {error}")),
        ),
        v6: None,
        max_slots: 2,
        limit: None,
    }
}

#[tokio::test]
async fn a_bound_client_connects_from_its_links_address() {
    let origin = Origin::start(OriginOptions::default()).await;
    for ip in ["127.0.0.2", "127.0.0.3"] {
        let link = loopback_link(ip);
        let addr: IpAddr = ip.parse().unwrap();
        let client =
            build_bound_client(&EngineConfig::default(), &link, &BindMode::Address(addr)).unwrap();
        // `127.0.0.1` is the origin's own address, so the URL needs no
        // DNS: the resolver filter is exercised by the unit tests.
        client.get(origin.url()).send().await.unwrap();
        let seen = origin.requests();
        assert_eq!(
            seen.last().unwrap().from.ip(),
            addr,
            "wrong source for {ip}"
        );
    }
    origin.shutdown().await;
}
