//! The collect stage (`ARCHITECTURE.md` §5, stages 1–3): turn pasted
//! input into probed, reviewable `PackageDraft`s *before* anything is
//! queued — JDownloader's "LinkGrabber" step — so a shell can show sizes
//! and names, let the user drop or regroup files, and then `enqueue`.
//!
//! Probing runs on a bounded pool: at most `max_concurrent_probes`
//! requests in flight overall and `max_probes_per_host` to one host, so
//! pasting hundreds of links never opens hundreds of connections.

use crate::config::EngineConfig;
use crate::group::group_names;
use crate::model::ArchiveSet;
use crate::probe::{self, ProbeResult};
use crate::resolve::{Registry, ResolveContext};
use crate::scan;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// One collected file: where it came from and what probing found.
#[derive(Debug, Clone, PartialEq)]
pub struct DraftFile {
    pub source: String,
    /// The probe result, or a human-readable reason it failed (an
    /// unsupported scheme, an unreachable host, a `404`).
    pub probe: Result<ProbeResult, String>,
    /// The page it was found on, when page scanning found it (§13.1).
    pub found_on: Option<String>,
    /// That page's title, which names the package.
    pub page_title: Option<String>,
}

/// What `collect` does with links that turn out to be web pages (§13.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CollectOptions {
    /// Offer the files a page links to or embeds, instead of the page.
    pub scan_pages: bool,
    /// Also follow the page's other links on the same site, one level
    /// down, and offer the files found there.
    pub deep: bool,
}

/// A package waiting for the user's go-ahead. Archive sets (§12.1) each
/// get their own draft, files in volume order; every other file in the
/// batch shares one draft.
#[derive(Debug, Clone, PartialEq)]
pub struct PackageDraft {
    pub name: String,
    pub files: Vec<DraftFile>,
    /// Set when the files form (or are) an archive.
    pub archive: Option<ArchiveSet>,
    /// 1-based volumes the set is missing ("volume 7 missing").
    pub missing_volumes: Vec<u32>,
}

impl PackageDraft {
    /// Whether a shell should offer "extract after download" for this
    /// draft: it appears to be an archive.
    #[must_use]
    pub fn offers_extract(&self) -> bool {
        self.archive.is_some()
    }

    /// Files that probed successfully.
    #[must_use]
    pub fn ok_count(&self) -> usize {
        self.files.iter().filter(|f| f.probe.is_ok()).count()
    }
}

/// Resolves and probes `inputs`, keeping their order. Web pages are
/// offered as they are (a page is a file too); see `collect_with`.
pub async fn collect(
    inputs: Vec<String>,
    config: &EngineConfig,
    registry: &Registry,
    client: &reqwest::Client,
    cancel: &CancellationToken,
) -> Vec<PackageDraft> {
    collect_with(
        inputs,
        CollectOptions::default(),
        config,
        registry,
        client,
        cancel,
    )
    .await
}

/// A link waiting to be probed, and where it came from.
#[derive(Clone)]
struct Pending {
    source: String,
    url: Result<url::Url, String>,
    /// 0 for what the user pasted; 1 for what a page linked to; 2 for
    /// what a deep scan found one page further.
    depth: u8,
    /// Found by name as a file link, or pasted: a failure is worth
    /// showing. A deep scan's speculative page links fail silently.
    expected_file: bool,
    found_on: Option<String>,
    page_title: Option<String>,
}

/// Resolves and probes `inputs`; with `options.scan_pages`, a link that
/// is a web page is replaced by the files on it (§13.1).
pub async fn collect_with(
    inputs: Vec<String>,
    options: CollectOptions,
    config: &EngineConfig,
    registry: &Registry,
    client: &reqwest::Client,
    cancel: &CancellationToken,
) -> Vec<PackageDraft> {
    let ctx = ResolveContext {
        client: client.clone(),
        cancel: cancel.clone(),
    };

    // Stage 2: resolve every input to direct URLs (may expand or fail).
    let mut level: Vec<Pending> = Vec::new();
    for input in inputs {
        let input = input.trim().to_string();
        if input.is_empty() {
            continue;
        }
        match registry.resolve(&input, &ctx).await {
            Ok(links) => {
                for link in links {
                    level.push(Pending {
                        source: link.url.to_string(),
                        url: Ok(link.url),
                        depth: 0,
                        expected_file: true,
                        found_on: None,
                        page_title: None,
                    });
                }
            }
            Err(error) => level.push(Pending {
                source: input,
                url: Err(error.to_string()),
                depth: 0,
                expected_file: true,
                found_on: None,
                page_title: None,
            }),
        }
    }

    // Stage 3: probe, level by level; pages found on the way are scanned
    // (§13.1) and what they link to becomes the next level. Everything in
    // a level runs at once, on the bounded pools.
    let deepest_scanned = if options.deep { 1 } else { 0 };
    let mut walk = Walk {
        seen: level.iter().map(|p| p.source.clone()).collect(),
        next: Vec::new(),
        files: Vec::new(),
        deep_pages: 0,
        deep: options.deep,
    };
    let mut config = config.clone();
    if options.scan_pages {
        // A scan probes whole albums; a few more at a host than for a
        // handful of pasted links (Decision 35).
        config.max_probes_per_host = config.max_probes_per_host.max(4);
    }
    let config = &config;
    while !level.is_empty() && !cancel.is_cancelled() {
        // A page link found on a page is opened directly, in one request:
        // if it isn't a page after all, it is probed as a file below.
        let (direct, to_probe): (Vec<Pending>, Vec<Pending>) = level
            .into_iter()
            .partition(|p| options.scan_pages && p.depth > 0 && !p.expected_file && p.url.is_ok());
        let direct_urls: Vec<url::Url> = direct.iter().filter_map(|p| p.url.clone().ok()).collect();
        let (probed, opened) = tokio::join!(
            probe_all(&to_probe, config, client),
            fetch_all(&direct_urls, config, client, cancel),
        );

        // Pasted links and file links: files, or pages to open.
        let mut pages: Vec<(Pending, url::Url)> = Vec::new();
        for (pending, probe) in to_probe.into_iter().zip(probed) {
            let is_page = probe.as_ref().is_ok_and(ProbeResult::is_html);
            if options.scan_pages && is_page {
                if pending.depth <= deepest_scanned
                    && let Ok(probe) = probe
                {
                    pages.push((pending, probe.final_url));
                }
            } else if probe.is_ok() || pending.expected_file {
                // A page link that isn't scanned (scanning is off) is not
                // a file anyone asked for.
                if is_page && pending.depth > 0 {
                    continue;
                }
                walk.files.push(DraftFile {
                    source: pending.source,
                    probe,
                    found_on: pending.found_on,
                    page_title: pending.page_title,
                });
            }
        }

        // Page links opened directly: scan them, or treat a link that
        // turned out to be a file as one.
        let mut late_files: Vec<Pending> = Vec::new();
        for (pending, result) in direct.into_iter().zip(opened) {
            match result {
                Ok((url, page)) => walk.absorb(&pending, &url, page),
                Err(scan::FetchError::NotAPage) => late_files.push(pending),
                Err(scan::FetchError::Failed(_)) => {}
            }
        }

        // Pages the probe found: open them, all at once.
        let page_urls: Vec<url::Url> = pages.iter().map(|(_, url)| url.clone()).collect();
        for ((pending, _), result) in pages
            .into_iter()
            .zip(fetch_all(&page_urls, config, client, cancel).await)
        {
            match result {
                Ok((url, page)) => walk.absorb(&pending, &url, page),
                Err(error) if pending.expected_file => walk.files.push(DraftFile {
                    source: pending.source,
                    probe: Err(error.to_string()),
                    found_on: pending.found_on,
                    page_title: pending.page_title,
                }),
                Err(_) => {}
            }
        }

        if !late_files.is_empty() {
            for (pending, probe) in late_files
                .iter()
                .zip(probe_all(&late_files, config, client).await)
            {
                if probe.is_ok_and_not_html() {
                    walk.files.push(DraftFile {
                        source: pending.source.clone(),
                        probe,
                        found_on: pending.found_on.clone(),
                        page_title: pending.page_title.clone(),
                    });
                }
            }
        }
        level = std::mem::take(&mut walk.next);
    }
    let files = walk.files;
    if files.is_empty() {
        return Vec::new();
    }

    let names: Vec<String> = files.iter().map(file_name).collect();
    let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut slots: Vec<Option<DraftFile>> = files.into_iter().map(Some).collect();
    let mut drafts = Vec::new();
    for group in group_names(&name_refs) {
        let members: Vec<DraftFile> = group
            .members
            .iter()
            .filter_map(|&i| slots[i].take())
            .collect();
        let shared_title = members
            .first()
            .and_then(|f| f.page_title.clone())
            .filter(|title| {
                members
                    .iter()
                    .all(|f| f.page_title.as_deref() == Some(title))
            });
        let name = match (&group.archive, shared_title) {
            (Some(archive), _) => archive.base_name.clone(),
            (None, Some(title)) => crate::probe::sanitize_filename(&title),
            (None, None) => members
                .iter()
                .find_map(|f| f.probe.as_ref().ok().map(|p| p.suggested_name.clone()))
                .unwrap_or_else(|| "download".to_string()),
        };
        drafts.push(PackageDraft {
            name,
            files: members,
            archive: group.archive,
            missing_volumes: group.missing,
        });
    }
    drafts
}

/// What the scan has gathered so far, and the links it still has to look
/// at.
struct Walk {
    seen: HashSet<String>,
    /// The next level: file links to probe, page links to open.
    next: Vec<Pending>,
    files: Vec<DraftFile>,
    deep_pages: usize,
    deep: bool,
}

impl Walk {
    /// Takes what a scanned page offers: its file links, and — for a deep
    /// scan of a page the user pasted — its same-site page links.
    fn absorb(&mut self, pending: &Pending, page_url: &url::Url, page: scan::Page) {
        let from = page_url.to_string();
        // The page the user pasted names the package, not the file pages a
        // deep scan went through.
        let title = pending.page_title.clone().or_else(|| page.title.clone());
        for link in page.links {
            let is_file = scan::is_file_link(&link);
            let follow = !is_file
                && self.deep
                && pending.depth == 0
                && self.deep_pages < scan::MAX_DEEP_PAGES
                && scan::same_site(page_url, &link);
            if !is_file && !follow {
                continue;
            }
            if !self.seen.insert(link.to_string()) {
                continue;
            }
            if follow {
                self.deep_pages += 1;
            }
            self.next.push(Pending {
                source: link.to_string(),
                url: Ok(link),
                depth: pending.depth + 1,
                expected_file: is_file,
                found_on: Some(from.clone()),
                page_title: title.clone(),
            });
        }
    }
}

/// Opens `urls` as pages on the bounded pools, results in the same order
/// (§13.1): at most `max_concurrent_probes` at once, `PAGES_PER_HOST` to
/// one host.
async fn fetch_all(
    urls: &[url::Url],
    config: &EngineConfig,
    client: &reqwest::Client,
    cancel: &CancellationToken,
) -> Vec<Result<(url::Url, scan::Page), scan::FetchError>> {
    let total = Arc::new(Semaphore::new(config.max_concurrent_probes.max(1) as usize));
    let mut per_host: HashMap<String, Arc<Semaphore>> = HashMap::new();
    let mut tasks = tokio::task::JoinSet::new();
    for (index, url) in urls.iter().cloned().enumerate() {
        let host = url.host_str().unwrap_or_default().to_string();
        let host_pool = per_host
            .entry(host)
            .or_insert_with(|| Arc::new(Semaphore::new(scan::PAGES_PER_HOST)))
            .clone();
        let total = total.clone();
        let client = client.clone();
        let cancel = cancel.clone();
        tasks.spawn(async move {
            let _host_permit = host_pool.acquire_owned().await;
            let _permit = total.acquire_owned().await;
            (index, scan::fetch_page(&client, &url, &cancel).await)
        });
    }
    let mut results: Vec<Option<_>> = urls.iter().map(|_| None).collect();
    while let Some(joined) = tasks.join_next().await {
        if let Ok((index, result)) = joined {
            results[index] = Some(result);
        }
    }
    results
        .into_iter()
        .map(|r| r.unwrap_or_else(|| Err(scan::FetchError::Failed("interrupted".to_string()))))
        .collect()
}

trait ProbeOutcome {
    fn is_ok_and_not_html(&self) -> bool;
}

impl ProbeOutcome for Result<ProbeResult, String> {
    fn is_ok_and_not_html(&self) -> bool {
        self.as_ref().is_ok_and(|probe| !probe.is_html())
    }
}

/// Probes `level` on the bounded pool (§5), results in the same order.
async fn probe_all(
    level: &[Pending],
    config: &EngineConfig,
    client: &reqwest::Client,
) -> Vec<Result<ProbeResult, String>> {
    let total = Arc::new(Semaphore::new(config.max_concurrent_probes.max(1) as usize));
    let mut per_host: HashMap<String, Arc<Semaphore>> = HashMap::new();
    let mut tasks = tokio::task::JoinSet::new();
    for (index, pending) in level.iter().enumerate() {
        let url = match &pending.url {
            Ok(url) => url.clone(),
            Err(reason) => {
                let reason = reason.clone();
                tasks.spawn(async move { (index, Err(reason)) });
                continue;
            }
        };
        let host = url.host_str().unwrap_or_default().to_string();
        let host_pool = per_host
            .entry(host)
            .or_insert_with(|| Arc::new(Semaphore::new(config.max_probes_per_host.max(1) as usize)))
            .clone();
        let total = total.clone();
        let client = client.clone();
        let config = config.clone();
        tasks.spawn(async move {
            // Host first, then the global pool, so a crowded host's
            // waiting probes never hold global permits idle.
            let _host_permit = host_pool.acquire_owned().await;
            let _permit = total.acquire_owned().await;
            let result = probe::probe(&client, url, &config)
                .await
                .map_err(|error| error.to_string());
            (index, result)
        });
    }
    let mut results: Vec<Option<Result<ProbeResult, String>>> = vec![None; level.len()];
    while let Some(joined) = tasks.join_next().await {
        if let Ok((index, result)) = joined {
            results[index] = Some(result);
        }
    }
    results
        .into_iter()
        .map(|r| r.unwrap_or_else(|| Err("the check was interrupted".to_string())))
        .collect()
}

/// The name grouping sees: the probed name, or for a failed probe the
/// last path segment of the source.
fn file_name(file: &DraftFile) -> String {
    match &file.probe {
        Ok(probe) => probe.suggested_name.clone(),
        Err(_) => url::Url::parse(&file.source)
            .ok()
            .and_then(|u| u.path_segments()?.next_back().map(str::to_string))
            .unwrap_or_else(|| file.source.clone()),
    }
}
