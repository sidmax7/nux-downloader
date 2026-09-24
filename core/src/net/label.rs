//! Friendly link labels (`ARCHITECTURE.md` §6.1): best-effort enrichers
//! that refine a discovered link's `label` and `kind` using OS facilities
//! (`/sys/class/net`, `networksetup`, `Get-NetAdapter`). Nothing here is
//! required — discovery already produced usable links — so every failure,
//! whether an error, a panic or a hang, just leaves the link as it was.
//!
//! The parsers are pure functions over text so they're tested on every
//! platform; only the thin command runners are OS-specific.

use crate::model::{Link, LinkId, LinkKind};
use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

/// What an enricher learned about one link. `None` fields leave the
/// link's current value alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrichment {
    pub link: LinkId,
    pub label: Option<String>,
    pub kind: Option<LinkKind>,
}

/// A best-effort source of friendly names. Blocking (it may run a
/// command or read `/sys`); `enrich_links` runs it off the async
/// runtime under a timeout.
pub trait LabelEnricher: Send + Sync {
    fn enrich(&self, links: &[Link]) -> io::Result<Vec<Enrichment>>;
}

/// Runs every enricher against `links`, each under `timeout`, and
/// applies whatever comes back. An enricher that errors, panics or
/// overruns the timeout is skipped; the links are always returned.
pub async fn enrich_links(
    mut links: Vec<Link>,
    enrichers: &[Arc<dyn LabelEnricher>],
    timeout: Duration,
) -> Vec<Link> {
    for enricher in enrichers {
        let enricher = Arc::clone(enricher);
        let snapshot = links.clone();
        let task = tokio::task::spawn_blocking(move || enricher.enrich(&snapshot));
        match tokio::time::timeout(timeout, task).await {
            Ok(Ok(Ok(found))) => apply(&mut links, &found),
            Ok(Ok(Err(error))) => tracing::debug!(%error, "label enricher failed"),
            Ok(Err(error)) => tracing::debug!(%error, "label enricher panicked"),
            Err(_) => tracing::debug!("label enricher timed out"),
        }
    }
    links
}

fn apply(links: &mut [Link], found: &[Enrichment]) {
    for enrichment in found {
        let Some(link) = links.iter_mut().find(|l| l.id == enrichment.link) else {
            continue;
        };
        if let Some(label) = enrichment.label.as_ref().filter(|l| !l.trim().is_empty()) {
            link.label.clone_from(label);
        }
        if let Some(kind) = enrichment.kind {
            link.kind = kind;
        }
    }
}

/// The enrichers that make sense on the platform this was built for.
#[must_use]
pub fn system_enrichers() -> Vec<Arc<dyn LabelEnricher>> {
    if cfg!(target_os = "linux") {
        vec![Arc::new(SysfsEnricher::default())]
    } else if cfg!(target_os = "macos") {
        vec![Arc::new(NetworksetupEnricher)]
    } else if cfg!(windows) {
        vec![Arc::new(NetAdapterEnricher)]
    } else {
        Vec::new()
    }
}

// ---- Linux: /sys/class/net -------------------------------------------

/// Reads `<root>/<iface>/` — `wireless/` marks Wi-Fi, `bridge/` a
/// bridge, and the `device/driver` symlink's name identifies USB
/// tethering drivers. `root` is injectable for tests.
#[derive(Debug, Clone)]
pub struct SysfsEnricher {
    root: PathBuf,
}

impl SysfsEnricher {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl Default for SysfsEnricher {
    fn default() -> Self {
        Self::new("/sys/class/net")
    }
}

const USB_TETHER_DRIVERS: &[&str] = &["rndis_host", "cdc_ether", "cdc_ncm", "ipheth"];

impl LabelEnricher for SysfsEnricher {
    fn enrich(&self, links: &[Link]) -> io::Result<Vec<Enrichment>> {
        // A missing root means this isn't a sysfs system: fail the whole
        // enricher rather than report nothing for each link.
        std::fs::metadata(&self.root)?;
        let mut found = Vec::new();
        for link in links {
            let dir = self.root.join(&link.name);
            let driver = std::fs::read_link(dir.join("device/driver"))
                .ok()
                .and_then(|target| target.file_name().map(|n| n.to_string_lossy().into_owned()));
            let kind = if dir.join("wireless").exists() {
                Some(LinkKind::Wifi)
            } else if dir.join("bridge").exists() {
                Some(LinkKind::Bridge)
            } else if driver
                .as_deref()
                .is_some_and(|d| USB_TETHER_DRIVERS.contains(&d))
            {
                Some(LinkKind::UsbTether)
            } else {
                None
            };
            if let Some(kind) = kind {
                found.push(Enrichment {
                    link: link.id,
                    label: Some(format!("{} ({})", kind_word(kind), link.name)),
                    kind: Some(kind),
                });
            }
        }
        Ok(found)
    }
}

fn kind_word(kind: LinkKind) -> &'static str {
    match kind {
        LinkKind::Wifi => "Wi-Fi",
        LinkKind::Ethernet => "Ethernet",
        LinkKind::UsbTether => "USB tether",
        LinkKind::Bridge => "Bridge",
        LinkKind::Other => "Network",
    }
}

// ---- macOS: networksetup ---------------------------------------------

/// `(hardware port name, device)` pairs from
/// `networksetup -listallhardwareports`.
#[must_use]
pub fn parse_networksetup(output: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut port: Option<&str> = None;
    for line in output.lines() {
        if let Some(name) = line.strip_prefix("Hardware Port:") {
            port = Some(name.trim());
        } else if let Some(device) = line.strip_prefix("Device:")
            && let Some(port) = port.take()
        {
            let device = device.trim();
            if !device.is_empty() {
                pairs.push((port.to_string(), device.to_string()));
            }
        }
    }
    pairs
}

fn kind_from_port(port: &str) -> Option<LinkKind> {
    let p = port.to_ascii_lowercase();
    if p.contains("wi-fi") || p.contains("wifi") || p.contains("airport") {
        Some(LinkKind::Wifi)
    } else if p.contains("iphone") || p.contains("usb") || p.contains("tether") {
        Some(LinkKind::UsbTether)
    } else if p.contains("bridge") {
        Some(LinkKind::Bridge)
    } else if p.contains("ethernet") || p.contains("thunderbolt") || p.contains("lan") {
        Some(LinkKind::Ethernet)
    } else {
        None
    }
}

fn enrichments_from_ports(links: &[Link], ports: &[(String, String)]) -> Vec<Enrichment> {
    links
        .iter()
        .filter_map(|link| {
            let (port, _) = ports.iter().find(|(_, device)| *device == link.name)?;
            Some(Enrichment {
                link: link.id,
                label: Some(port.clone()),
                kind: kind_from_port(port),
            })
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
pub struct NetworksetupEnricher;

impl LabelEnricher for NetworksetupEnricher {
    fn enrich(&self, links: &[Link]) -> io::Result<Vec<Enrichment>> {
        let output = run("networksetup", &["-listallhardwareports"])?;
        Ok(enrichments_from_ports(links, &parse_networksetup(&output)))
    }
}

// ---- Windows: Get-NetAdapter -----------------------------------------

/// `(name, description)` from the `Name|InterfaceDescription` lines the
/// PowerShell command below prints.
#[must_use]
pub fn parse_net_adapters(output: &str) -> Vec<(String, String)> {
    output
        .lines()
        .filter_map(|line| {
            let (name, description) = line.split_once('|')?;
            let (name, description) = (name.trim(), description.trim());
            (!name.is_empty()).then(|| (name.to_string(), description.to_string()))
        })
        .collect()
}

fn kind_from_adapter(name: &str, description: &str) -> Option<LinkKind> {
    let text = format!("{name} {description}").to_ascii_lowercase();
    if text.contains("wi-fi") || text.contains("wireless") || text.contains("wlan") {
        Some(LinkKind::Wifi)
    } else if text.contains("rndis") || text.contains("remote ndis") || text.contains("iphone") {
        Some(LinkKind::UsbTether)
    } else if text.contains("virtual") || text.contains("hyper-v") || text.contains("bridge") {
        Some(LinkKind::Bridge)
    } else if text.contains("ethernet") {
        Some(LinkKind::Ethernet)
    } else {
        None
    }
}

fn enrichments_from_adapters(links: &[Link], adapters: &[(String, String)]) -> Vec<Enrichment> {
    links
        .iter()
        .filter_map(|link| {
            let (name, description) = adapters.iter().find(|(name, _)| *name == link.name)?;
            Some(Enrichment {
                link: link.id,
                label: (!description.is_empty()).then(|| format!("{name} — {description}")),
                kind: kind_from_adapter(name, description),
            })
        })
        .collect()
}

/// Not verified on Windows: no Windows machine was available when this
/// was written. The parser is tested; the command itself is not.
#[derive(Debug, Clone, Copy)]
pub struct NetAdapterEnricher;

impl LabelEnricher for NetAdapterEnricher {
    fn enrich(&self, links: &[Link]) -> io::Result<Vec<Enrichment>> {
        let output = run(
            "powershell",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Get-NetAdapter | ForEach-Object { \"$($_.Name)|$($_.InterfaceDescription)\" }",
            ],
        )?;
        Ok(enrichments_from_adapters(
            links,
            &parse_net_adapters(&output),
        ))
    }
}

fn run(program: &str, args: &[&str]) -> io::Result<String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{program} exited with {}",
            output.status
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EngineConfig;
    use crate::net::discover::{RawAddr, links_from_addresses};

    fn links(names: &[&str]) -> Vec<Link> {
        let addrs: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(i, name)| RawAddr {
                name: (*name).to_string(),
                ip: format!("10.0.{i}.2").parse().unwrap(),
            })
            .collect();
        links_from_addresses(&addrs, &EngineConfig::default())
    }

    struct Failing;
    impl LabelEnricher for Failing {
        fn enrich(&self, _: &[Link]) -> io::Result<Vec<Enrichment>> {
            Err(io::Error::other("nope"))
        }
    }

    struct Panicking;
    impl LabelEnricher for Panicking {
        fn enrich(&self, _: &[Link]) -> io::Result<Vec<Enrichment>> {
            panic!("enricher bug");
        }
    }

    struct Hanging;
    impl LabelEnricher for Hanging {
        fn enrich(&self, _: &[Link]) -> io::Result<Vec<Enrichment>> {
            std::thread::sleep(Duration::from_millis(500));
            Ok(Vec::new())
        }
    }

    struct Fixed(Vec<Enrichment>);
    impl LabelEnricher for Fixed {
        fn enrich(&self, _: &[Link]) -> io::Result<Vec<Enrichment>> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn links_survive_every_enricher_failing() {
        let before = links(&["eth0", "wlan0"]);
        let enrichers: Vec<Arc<dyn LabelEnricher>> =
            vec![Arc::new(Failing), Arc::new(Panicking), Arc::new(Hanging)];
        let after = enrich_links(before.clone(), &enrichers, Duration::from_millis(100)).await;
        assert_eq!(after, before);
    }

    #[tokio::test]
    async fn a_failing_enricher_does_not_block_a_working_one() {
        let before = links(&["eth0"]);
        let good = Fixed(vec![Enrichment {
            link: before[0].id,
            label: Some("Office LAN".into()),
            kind: Some(LinkKind::Ethernet),
        }]);
        let enrichers: Vec<Arc<dyn LabelEnricher>> = vec![Arc::new(Failing), Arc::new(good)];
        let after = enrich_links(before, &enrichers, Duration::from_secs(1)).await;
        assert_eq!(after[0].label, "Office LAN");
    }

    #[tokio::test]
    async fn blank_labels_and_unknown_links_are_ignored() {
        let before = links(&["eth0"]);
        let odd = Fixed(vec![
            Enrichment {
                link: before[0].id,
                label: Some("  ".into()),
                kind: None,
            },
            Enrichment {
                link: LinkId::new(1),
                label: Some("ghost".into()),
                kind: Some(LinkKind::Wifi),
            },
        ]);
        let enrichers: Vec<Arc<dyn LabelEnricher>> = vec![Arc::new(odd)];
        let after = enrich_links(before.clone(), &enrichers, Duration::from_secs(1)).await;
        assert_eq!(after, before);
    }

    #[test]
    fn sysfs_recognises_wifi_bridge_and_usb_tether() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("wlx1/wireless")).unwrap();
        std::fs::create_dir_all(root.join("dock0/bridge")).unwrap();
        std::fs::create_dir_all(root.join("phone0/device")).unwrap();
        std::fs::create_dir_all(root.join("plain0")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            "../../../bus/usb/drivers/rndis_host",
            root.join("phone0/device/driver"),
        )
        .unwrap();

        let found = SysfsEnricher::new(root)
            .enrich(&links(&["dock0", "phone0", "plain0", "wlx1"]))
            .unwrap();
        let kinds: Vec<_> = found.iter().map(|e| e.kind.unwrap()).collect();
        #[cfg(unix)]
        assert_eq!(
            kinds,
            [LinkKind::Bridge, LinkKind::UsbTether, LinkKind::Wifi]
        );
        #[cfg(not(unix))]
        assert_eq!(kinds, [LinkKind::Bridge, LinkKind::Wifi]);
    }

    #[test]
    fn sysfs_errors_when_the_root_is_missing() {
        let enricher = SysfsEnricher::new("/definitely/not/here");
        assert!(enricher.enrich(&links(&["eth0"])).is_err());
    }

    #[test]
    fn networksetup_output_is_parsed_and_matched_by_device() {
        let output = "\
Hardware Port: Wi-Fi
Device: en0
Ethernet Address: aa:bb

Hardware Port: iPhone USB
Device: en6
Ethernet Address: cc:dd

Hardware Port: Thunderbolt Bridge
Device: bridge0
";
        let ports = parse_networksetup(output);
        assert_eq!(ports.len(), 3);
        let found = enrichments_from_ports(&links(&["en0", "en6", "en9"]), &ports);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].label.as_deref(), Some("Wi-Fi"));
        assert_eq!(found[0].kind, Some(LinkKind::Wifi));
        assert_eq!(found[1].kind, Some(LinkKind::UsbTether));
    }

    #[test]
    fn get_netadapter_output_is_parsed_and_matched_by_name() {
        let output = "Wi-Fi|Intel(R) Wi-Fi 6 AX201\nEthernet|Realtek PCIe GbE\nGarbage line\n";
        let adapters = parse_net_adapters(output);
        assert_eq!(adapters.len(), 2);
        let found = enrichments_from_adapters(&links(&["Ethernet", "Wi-Fi"]), &adapters);
        assert_eq!(found[0].kind, Some(LinkKind::Ethernet));
        assert_eq!(found[1].kind, Some(LinkKind::Wifi));
        assert_eq!(
            found[1].label.as_deref(),
            Some("Wi-Fi — Intel(R) Wi-Fi 6 AX201")
        );
    }
}
