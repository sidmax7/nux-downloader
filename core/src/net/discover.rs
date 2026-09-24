//! Link discovery (`ARCHITECTURE.md` §6.1): turn the OS's interface
//! addresses into `Link`s. The grouping and filtering is pure — it works
//! on a plain list of `(interface name, address)` pairs behind the
//! `LinkSource` trait — so tests inject fake interface lists instead of
//! depending on the machine they run on.

use crate::config::EngineConfig;
use crate::model::{Link, LinkId, LinkKind};
use std::collections::BTreeMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// One address on one interface, as the OS reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawAddr {
    pub name: String,
    pub ip: IpAddr,
}

/// Where interface addresses come from. `SystemLinkSource` is the real
/// one; tests supply their own.
pub trait LinkSource {
    fn addresses(&self) -> io::Result<Vec<RawAddr>>;
}

/// Reads the machine's interfaces via `if-addrs`, on every platform.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemLinkSource;

impl LinkSource for SystemLinkSource {
    fn addresses(&self) -> io::Result<Vec<RawAddr>> {
        Ok(if_addrs::get_if_addrs()?
            .into_iter()
            .map(|interface| RawAddr {
                ip: interface.ip(),
                name: interface.name,
            })
            .collect())
    }
}

fn is_link_local_v4(ip: Ipv4Addr) -> bool {
    ip.is_link_local()
}

fn is_link_local_v6(ip: Ipv6Addr) -> bool {
    ip.segments()[0] & 0xffc0 == 0xfe80
}

fn usable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => !ip.is_loopback() && !ip.is_unspecified() && !is_link_local_v4(ip),
        IpAddr::V6(ip) => !ip.is_loopback() && !ip.is_unspecified() && !is_link_local_v6(ip),
    }
}

/// A stable id derived from the interface name (FNV-1a), so the same
/// interface keeps the same `LinkId` across rescans and restarts (§6.1).
#[must_use]
pub fn link_id_for(name: &str) -> LinkId {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    LinkId::new(hash)
}

/// A best-guess kind from the interface name alone (label enrichers,
/// P3.3, can refine it). Names differ per OS; this only recognizes the
/// common conventions and falls back to `Other`.
#[must_use]
pub fn kind_from_name(name: &str) -> LinkKind {
    let n = name.to_ascii_lowercase();
    let starts = |prefixes: &[&str]| prefixes.iter().any(|p| n.starts_with(p));
    if starts(&["wl", "wifi", "wlan", "ath", "wlp"]) {
        LinkKind::Wifi
    } else if starts(&["usb", "rndis", "enx", "ppp", "rmnet"]) {
        LinkKind::UsbTether
    } else if starts(&[
        "br", "docker", "virbr", "veth", "vmnet", "vboxnet", "bridge",
    ]) {
        LinkKind::Bridge
    } else if starts(&["eth", "enp", "eno", "ens"]) {
        LinkKind::Ethernet
    } else {
        LinkKind::Other
    }
}

/// Groups usable addresses into links: no loopback, no unspecified, no
/// link-local; one link per interface name, carrying its first IPv4 and
/// first IPv6 address. Sorted by name so the order is stable.
#[must_use]
pub fn links_from_addresses(addresses: &[RawAddr], config: &EngineConfig) -> Vec<Link> {
    let mut by_name: BTreeMap<&str, (Option<Ipv4Addr>, Option<Ipv6Addr>)> = BTreeMap::new();
    for addr in addresses.iter().filter(|addr| usable(addr.ip)) {
        let entry = by_name.entry(addr.name.as_str()).or_default();
        match addr.ip {
            IpAddr::V4(ip) => {
                entry.0.get_or_insert(ip);
            }
            IpAddr::V6(ip) => {
                entry.1.get_or_insert(ip);
            }
        }
    }
    by_name
        .into_iter()
        .map(|(name, (v4, v6))| Link {
            id: link_id_for(name),
            name: name.to_string(),
            label: name.to_string(),
            kind: kind_from_name(name),
            v4,
            v6,
            max_slots: config.slots_per_link,
            limit: None,
        })
        .collect()
}

/// Discovers the links `source` currently offers.
pub fn discover(source: &dyn LinkSource, config: &EngineConfig) -> io::Result<Vec<Link>> {
    Ok(links_from_addresses(&source.addresses()?, config))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(name: &str, ip: &str) -> RawAddr {
        RawAddr {
            name: name.to_string(),
            ip: ip.parse().unwrap(),
        }
    }

    struct Fake(Vec<RawAddr>);
    impl LinkSource for Fake {
        fn addresses(&self) -> io::Result<Vec<RawAddr>> {
            Ok(self.0.clone())
        }
    }

    struct Broken;
    impl LinkSource for Broken {
        fn addresses(&self) -> io::Result<Vec<RawAddr>> {
            Err(io::Error::other("no interfaces"))
        }
    }

    #[test]
    fn loopback_link_local_and_unspecified_are_dropped() {
        let links = links_from_addresses(
            &[
                raw("lo", "127.0.0.1"),
                raw("lo", "::1"),
                raw("wlan0", "169.254.3.4"),
                raw("wlan0", "fe80::1"),
                raw("eth9", "0.0.0.0"),
            ],
            &EngineConfig::default(),
        );
        assert!(links.is_empty());
    }

    #[test]
    fn an_interface_with_both_families_is_one_link() {
        let links = links_from_addresses(
            &[
                raw("wlan0", "fe80::1"),
                raw("wlan0", "192.168.1.5"),
                raw("wlan0", "2001:db8::5"),
                raw("wlan0", "192.168.1.6"),
            ],
            &EngineConfig::default(),
        );
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].v4, Some("192.168.1.5".parse().unwrap()));
        assert_eq!(links[0].v6, Some("2001:db8::5".parse().unwrap()));
        assert_eq!(links[0].kind, LinkKind::Wifi);
    }

    #[test]
    fn links_are_sorted_by_name_and_get_the_configured_slots() {
        let config = EngineConfig {
            slots_per_link: 3,
            ..EngineConfig::default()
        };
        let links =
            links_from_addresses(&[raw("usb0", "10.0.0.2"), raw("eth0", "10.1.0.2")], &config);
        let names: Vec<_> = links.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["eth0", "usb0"]);
        assert!(links.iter().all(|l| l.max_slots == 3));
        assert_eq!(links[0].kind, LinkKind::Ethernet);
        assert_eq!(links[1].kind, LinkKind::UsbTether);
    }

    #[test]
    fn ids_are_stable_and_distinct() {
        assert_eq!(link_id_for("wlan0"), link_id_for("wlan0"));
        assert_ne!(link_id_for("wlan0"), link_id_for("usb0"));
    }

    #[test]
    fn kinds_from_common_names() {
        assert_eq!(kind_from_name("wlp3s0"), LinkKind::Wifi);
        assert_eq!(kind_from_name("enx0011"), LinkKind::UsbTether);
        assert_eq!(kind_from_name("docker0"), LinkKind::Bridge);
        assert_eq!(kind_from_name("enp2s0"), LinkKind::Ethernet);
        assert_eq!(kind_from_name("en0"), LinkKind::Other);
    }

    #[test]
    fn discover_uses_the_injected_source_and_propagates_errors() {
        let links = discover(
            &Fake(vec![raw("eth0", "10.0.0.1")]),
            &EngineConfig::default(),
        )
        .unwrap();
        assert_eq!(links.len(), 1);
        assert!(discover(&Broken, &EngineConfig::default()).is_err());
    }

    #[test]
    fn the_real_source_does_not_error() {
        assert!(discover(&SystemLinkSource, &EngineConfig::default()).is_ok());
    }
}
