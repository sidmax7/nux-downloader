//! The link monitor (`ARCHITECTURE.md` §6.1): rescan the machine's
//! interfaces every few seconds and tell the engine when the set of
//! usable links changes. The engine reacts to the diff itself
//! (`EngineState::handle` of `Input::LinksChanged`) — this module only
//! produces scans.

use super::bind::BindPolicy;
use super::discover::{LinkSource, discover};
use super::label::{LabelEnricher, enrich_links};
use crate::config::EngineConfig;
use crate::engine::Input;
use crate::model::Link;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// How often the interface list is re-read (§6.1: "it's cheap").
pub const DEFAULT_RESCAN_INTERVAL: Duration = Duration::from_secs(5);
/// Each label enricher's budget per scan.
pub const DEFAULT_ENRICH_TIMEOUT: Duration = Duration::from_secs(2);

/// Where the driver's links come from.
#[derive(Clone)]
pub enum LinkSetup {
    /// A fixed list, never rescanned. `BindPolicy::Off` is "one link, let
    /// the OS route"; `Address` pins loopback-alias test links.
    Fixed { links: Vec<Link>, bind: BindPolicy },
    /// Discover links and keep watching for changes.
    Monitored(MonitorSetup),
}

#[derive(Clone)]
pub struct MonitorSetup {
    pub source: Arc<dyn LinkSource>,
    pub enrichers: Vec<Arc<dyn LabelEnricher>>,
    pub interval: Duration,
    pub enrich_timeout: Duration,
    /// How each link's connections are pinned (`net::bind`). `Off` only
    /// in tests, whose fake links have no real interface behind them.
    pub bind: BindPolicy,
    /// Use only the links with these interface names (`--links`).
    /// `None` uses every link found.
    pub only: Option<Vec<String>>,
}

impl MonitorSetup {
    /// The real machine: `if-addrs`, the platform's enrichers, bound
    /// clients.
    #[must_use]
    pub fn system() -> Self {
        Self {
            source: Arc::new(super::discover::SystemLinkSource),
            enrichers: super::label::system_enrichers(),
            interval: DEFAULT_RESCAN_INTERVAL,
            enrich_timeout: DEFAULT_ENRICH_TIMEOUT,
            bind: BindPolicy::Auto,
            only: None,
        }
    }
}

/// One scan: the links the source offers now. Labels already found for a
/// link in `previous` are carried over rather than re-enriched, so an
/// enricher that shells out (`powershell`) runs once per new link, not
/// every rescan.
pub async fn scan(
    setup: &MonitorSetup,
    config: &EngineConfig,
    previous: &[Link],
) -> io::Result<Vec<Link>> {
    let source = Arc::clone(&setup.source);
    let config = config.clone();
    let discovered = tokio::task::spawn_blocking(move || discover(source.as_ref(), &config))
        .await
        .map_err(io::Error::other)??;

    let discovered = match &setup.only {
        Some(names) => discovered
            .into_iter()
            .filter(|link| names.contains(&link.name))
            .collect(),
        None => discovered,
    };
    let (mut known, fresh): (Vec<Link>, Vec<Link>) = discovered
        .into_iter()
        .partition(|link| previous.iter().any(|p| p.id == link.id));
    for link in &mut known {
        if let Some(before) = previous.iter().find(|p| p.id == link.id) {
            link.label.clone_from(&before.label);
            link.kind = before.kind;
        }
    }
    let fresh = if fresh.is_empty() {
        fresh
    } else {
        enrich_links(fresh, &setup.enrichers, setup.enrich_timeout).await
    };
    known.extend(fresh);
    known.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(known)
}

/// Rescans every `setup.interval` and sends `Input::LinksChanged`
/// whenever the result differs from the last one. Holds only a weak
/// sender, so it never keeps a dropped driver alive, and ends when the
/// driver's input channel closes. A failed scan keeps the last known set.
pub fn spawn_watch(
    setup: MonitorSetup,
    config: EngineConfig,
    initial: Vec<Link>,
    input_tx: mpsc::WeakSender<Input>,
) {
    tokio::spawn(async move {
        let mut last = initial;
        let mut ticker = tokio::time::interval(setup.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // the initial scan already happened
        loop {
            ticker.tick().await;
            let links = match scan(&setup, &config, &last).await {
                Ok(links) => links,
                Err(error) => {
                    tracing::warn!(%error, "link rescan failed; keeping the last known links");
                    continue;
                }
            };
            if links == last {
                continue;
            }
            let Some(tx) = input_tx.upgrade() else {
                return;
            };
            if tx.send(Input::LinksChanged(links.clone())).await.is_err() {
                return;
            }
            last = links;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LinkKind;
    use crate::net::discover::RawAddr;
    use crate::net::label::Enrichment;
    use std::sync::Mutex;

    /// An interface list a test can edit while the monitor runs.
    #[derive(Default)]
    pub(crate) struct Shared(Mutex<Vec<RawAddr>>);
    impl Shared {
        pub(crate) fn set(&self, entries: &[(&str, &str)]) {
            *self.0.lock().unwrap() = entries
                .iter()
                .map(|(name, ip)| RawAddr {
                    name: (*name).to_string(),
                    ip: ip.parse().unwrap(),
                })
                .collect();
        }
    }
    impl LinkSource for Shared {
        fn addresses(&self) -> io::Result<Vec<RawAddr>> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    struct CountingEnricher(Mutex<u32>);
    impl LabelEnricher for CountingEnricher {
        fn enrich(&self, links: &[Link]) -> io::Result<Vec<Enrichment>> {
            *self.0.lock().unwrap() += 1;
            Ok(links
                .iter()
                .map(|l| Enrichment {
                    link: l.id,
                    label: Some(format!("nice {}", l.name)),
                    kind: Some(LinkKind::Ethernet),
                })
                .collect())
        }
    }

    fn setup(source: Arc<Shared>, enrichers: Vec<Arc<dyn LabelEnricher>>) -> MonitorSetup {
        MonitorSetup {
            source,
            enrichers,
            interval: Duration::from_millis(20),
            enrich_timeout: Duration::from_secs(1),
            bind: BindPolicy::Off,
            only: None,
        }
    }

    #[tokio::test]
    async fn labels_are_looked_up_once_per_new_link() {
        let shared = Arc::new(Shared::default());
        shared.set(&[("eth0", "10.0.0.2")]);
        let counter = Arc::new(CountingEnricher(Mutex::new(0)));
        let setup = setup(shared.clone(), vec![counter.clone()]);
        let config = EngineConfig::default();

        let first = scan(&setup, &config, &[]).await.unwrap();
        assert_eq!(first[0].label, "nice eth0");
        let second = scan(&setup, &config, &first).await.unwrap();
        assert_eq!(second, first);
        assert_eq!(*counter.0.lock().unwrap(), 1);

        shared.set(&[("eth0", "10.0.0.2"), ("usb0", "10.0.1.2")]);
        let third = scan(&setup, &config, &second).await.unwrap();
        assert_eq!(third.len(), 2);
        assert_eq!(*counter.0.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn only_keeps_the_named_links() {
        let shared = Arc::new(Shared::default());
        shared.set(&[
            ("eth0", "10.0.0.2"),
            ("usb0", "10.0.1.2"),
            ("wlan0", "10.0.2.2"),
        ]);
        let mut setup = setup(shared, Vec::new());
        setup.only = Some(vec!["usb0".into(), "wlan0".into(), "nope".into()]);
        let links = scan(&setup, &EngineConfig::default(), &[]).await.unwrap();
        let names: Vec<_> = links.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["usb0", "wlan0"]);
    }

    #[tokio::test]
    async fn the_watch_reports_changes_only() {
        let shared = Arc::new(Shared::default());
        shared.set(&[("eth0", "10.0.0.2")]);
        let setup = setup(shared.clone(), Vec::new());
        let config = EngineConfig::default();
        let initial = scan(&setup, &config, &[]).await.unwrap();

        let (tx, mut rx) = mpsc::channel(8);
        spawn_watch(setup, config, initial, tx.downgrade());

        // No change → nothing sent.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), rx.recv())
                .await
                .is_err()
        );

        shared.set(&[("eth0", "10.0.0.2"), ("wlan0", "10.0.2.2")]);
        let Some(Input::LinksChanged(links)) = rx.recv().await else {
            panic!("expected LinksChanged");
        };
        assert_eq!(links.len(), 2);

        shared.set(&[("wlan0", "10.0.2.2")]);
        let Some(Input::LinksChanged(links)) = rx.recv().await else {
            panic!("expected LinksChanged");
        };
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].name, "wlan0");
    }

    #[tokio::test]
    async fn the_watch_stops_when_the_driver_is_gone() {
        let shared = Arc::new(Shared::default());
        shared.set(&[("eth0", "10.0.0.2")]);
        let setup = setup(shared.clone(), Vec::new());
        let (tx, rx) = mpsc::channel(8);
        spawn_watch(setup, EngineConfig::default(), Vec::new(), tx.downgrade());
        drop(tx);
        drop(rx);
        // Nothing to assert beyond "doesn't hang or panic"; give it a
        // few ticks to notice.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
