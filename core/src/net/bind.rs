//! Binding connections to a link (`ARCHITECTURE.md` §6.2). Binding the
//! local IP alone isn't enough on Linux (the routing table picks the
//! outgoing interface), so on Linux and macOS a client binds to the
//! *device*; on Windows, which routes by source address, it binds to the
//! link's IP. A per-link DNS resolver then keeps a client from quietly
//! connecting to an address family the link doesn't have — hyper only
//! applies a local address whose family matches the destination, so an
//! IPv4-bound client that resolves an IPv6 address would otherwise
//! connect unbound, over the default route.

use super::client::base_client_builder;
use crate::config::EngineConfig;
use crate::error::NetError;
use crate::model::Link;
use reqwest::Client;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

/// How a link's connections are pinned to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindMode {
    /// `SO_BINDTODEVICE` (Linux) / `IP_BOUND_IF` (macOS).
    Device(String),
    /// Bind the local source address (Windows, and loopback test links).
    Address(IpAddr),
}

/// The platform's mode for `link`, chosen in this one place. `None` if
/// the platform needs an address and the link has none.
#[must_use]
pub fn default_bind_mode(link: &Link) -> Option<BindMode> {
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        Some(BindMode::Device(link.name.clone()))
    } else {
        link.v4
            .map(IpAddr::V4)
            .or_else(|| link.v6.map(IpAddr::V6))
            .map(BindMode::Address)
    }
}

/// Which address families a link can actually use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Families {
    pub v4: bool,
    pub v6: bool,
}

impl Families {
    #[must_use]
    pub fn of(link: &Link) -> Self {
        Self {
            v4: link.v4.is_some(),
            v6: link.v6.is_some(),
        }
    }

    #[must_use]
    pub fn allows(self, addr: &SocketAddr) -> bool {
        match addr {
            SocketAddr::V4(_) => self.v4,
            SocketAddr::V6(_) => self.v6,
        }
    }
}

/// Wraps a resolver, dropping every result in a family the link lacks.
/// Fails the lookup rather than returning an empty list, so a name that
/// only resolves to unusable addresses errors instead of hanging.
pub struct FamilyFilter<R> {
    inner: R,
    families: Families,
}

impl<R> FamilyFilter<R> {
    pub fn new(inner: R, families: Families) -> Self {
        Self { inner, families }
    }
}

impl<R: Resolve> Resolve for FamilyFilter<R> {
    fn resolve(&self, name: Name) -> Resolving {
        let families = self.families;
        let lookup = self.inner.resolve(name);
        Box::pin(async move {
            let kept: Vec<SocketAddr> =
                lookup.await?.filter(|addr| families.allows(addr)).collect();
            if kept.is_empty() {
                return Err("the name has no address in a family this link can use".into());
            }
            Ok(Box::new(kept.into_iter()) as Addrs)
        })
    }
}

/// The system resolver (`getaddrinfo` via tokio).
pub struct SystemResolver;

impl Resolve for SystemResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// A client bound to `link` by `mode`, with a resolver filtered to the
/// link's address families. Same transfer settings as every other client
/// (`net::client`), plus an idle pool sized to the link's slots.
pub fn build_bound_client(
    config: &EngineConfig,
    link: &Link,
    mode: &BindMode,
) -> Result<Client, NetError> {
    let mut builder = base_client_builder(config)
        .pool_max_idle_per_host(usize::from(link.max_slots))
        .dns_resolver(Arc::new(FamilyFilter::new(
            SystemResolver,
            Families::of(link),
        )));
    builder = match mode {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        BindMode::Device(name) => builder.interface(name),
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        BindMode::Device(_) => {
            return Err(NetError::NoBindAddress {
                link: link.name.clone(),
            });
        }
        BindMode::Address(ip) => builder.local_address(*ip),
    };
    builder
        .build()
        .map_err(|source| NetError::BuildClient { source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LinkId, LinkKind};

    fn link(v4: Option<&str>, v6: Option<&str>) -> Link {
        Link {
            id: LinkId::new(1),
            name: "test0".into(),
            label: "test0".into(),
            kind: LinkKind::Other,
            v4: v4.map(|ip| ip.parse().unwrap()),
            v6: v6.map(|ip| ip.parse().unwrap()),
            max_slots: 4,
            limit: None,
        }
    }

    struct Fixed(Vec<SocketAddr>);
    impl Resolve for Fixed {
        fn resolve(&self, _name: Name) -> Resolving {
            let addrs = self.0.clone();
            Box::pin(async move { Ok(Box::new(addrs.into_iter()) as Addrs) })
        }
    }

    async fn resolve_through(
        families: Families,
        addrs: &[&str],
    ) -> Result<Vec<SocketAddr>, String> {
        let inner = Fixed(addrs.iter().map(|a| a.parse().unwrap()).collect());
        let filter = FamilyFilter::new(inner, families);
        let name: Name = "example.test".parse().unwrap();
        filter
            .resolve(name)
            .await
            .map(Iterator::collect)
            .map_err(|error| error.to_string())
    }

    #[tokio::test]
    async fn an_ipv4_only_link_never_gets_an_ipv6_address() {
        let families = Families::of(&link(Some("10.0.0.2"), None));
        let kept = resolve_through(families, &["[2001:db8::1]:0", "93.184.216.34:0"])
            .await
            .unwrap();
        assert_eq!(kept, vec!["93.184.216.34:0".parse::<SocketAddr>().unwrap()]);
    }

    #[tokio::test]
    async fn an_ipv6_only_link_never_gets_an_ipv4_address() {
        let families = Families::of(&link(None, Some("2001:db8::2")));
        let kept = resolve_through(families, &["93.184.216.34:0", "[2001:db8::1]:0"])
            .await
            .unwrap();
        assert_eq!(kept, vec!["[2001:db8::1]:0".parse::<SocketAddr>().unwrap()]);
    }

    #[tokio::test]
    async fn a_name_with_only_unusable_families_fails_the_lookup() {
        let families = Families::of(&link(Some("10.0.0.2"), None));
        assert!(
            resolve_through(families, &["[2001:db8::1]:0"])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_dual_stack_link_keeps_both() {
        let families = Families::of(&link(Some("10.0.0.2"), Some("2001:db8::2")));
        let kept = resolve_through(families, &["[2001:db8::1]:0", "93.184.216.34:0"])
            .await
            .unwrap();
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn default_mode_matches_the_platform() {
        let mode = default_bind_mode(&link(Some("10.0.0.2"), None));
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert_eq!(mode, Some(BindMode::Device("test0".into())));
        } else {
            assert_eq!(mode, Some(BindMode::Address("10.0.0.2".parse().unwrap())));
        }
    }

    #[test]
    fn an_address_mode_client_builds() {
        let client = build_bound_client(
            &EngineConfig::default(),
            &link(Some("127.0.0.1"), None),
            &BindMode::Address("127.0.0.1".parse().unwrap()),
        );
        assert!(client.is_ok());
    }
}
