//! `Link`: a network interface usable for downloading (`ARCHITECTURE.md`
//! §6.1). Defined here as a domain type; `net::discover` (Phase 3)
//! produces instances of it from `if-addrs`.

use super::{ByteRate, LinkId};
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, Ipv6Addr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LinkKind {
    Wifi,
    Ethernet,
    UsbTether,
    Bridge,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    /// Stable identifier — the OS interface name (e.g. `wlan0`, `en6`).
    pub id: LinkId,
    /// The OS device name, same as `id.get()` reads as a string — kept
    /// separate because `LinkId` may stop being name-derived later.
    pub name: String,
    /// Friendly name if a `LabelEnricher` found one; otherwise equal to
    /// `name`.
    pub label: String,
    pub kind: LinkKind,
    pub v4: Option<Ipv4Addr>,
    pub v6: Option<Ipv6Addr>,
    /// Connections allowed on this link. Default 8 (`EngineConfig`).
    pub max_slots: u8,
    /// Optional per-link rate cap, e.g. for a metered phone tether.
    pub limit: Option<ByteRate>,
}

impl Link {
    /// A link is only usable if it has at least one routable address —
    /// this is what `net::discover` filters on before constructing one.
    #[must_use]
    pub fn has_address(&self) -> bool {
        self.v4.is_some() || self.v6.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Link {
        Link {
            id: LinkId::new(1),
            name: "wlan0".into(),
            label: "Wi-Fi".into(),
            kind: LinkKind::Wifi,
            v4: Some(Ipv4Addr::new(192, 168, 1, 40)),
            v6: None,
            max_slots: 8,
            limit: None,
        }
    }

    #[test]
    fn has_address_true_with_v4_only() {
        assert!(sample().has_address());
    }

    #[test]
    fn has_address_false_with_neither() {
        let mut link = sample();
        link.v4 = None;
        link.v6 = None;
        assert!(!link.has_address());
    }

    #[test]
    fn serde_round_trip() {
        let link = sample();
        let json = serde_json::to_string(&link).unwrap();
        let back: Link = serde_json::from_str(&json).unwrap();
        assert_eq!(back, link);
    }
}
