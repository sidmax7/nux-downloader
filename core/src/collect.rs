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
use std::collections::HashMap;
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

/// Resolves and probes `inputs`, keeping their order.
pub async fn collect(
    inputs: Vec<String>,
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
    let mut sources: Vec<(String, Result<url::Url, String>)> = Vec::new();
    for input in inputs {
        let input = input.trim().to_string();
        if input.is_empty() {
            continue;
        }
        match registry.resolve(&input, &ctx).await {
            Ok(links) => {
                for link in links {
                    sources.push((link.url.to_string(), Ok(link.url)));
                }
            }
            Err(error) => sources.push((input, Err(error.to_string()))),
        }
    }

    // Stage 3: probe on the bounded pool.
    let total = Arc::new(Semaphore::new(config.max_concurrent_probes.max(1) as usize));
    let mut per_host: HashMap<String, Arc<Semaphore>> = HashMap::new();
    let mut tasks = tokio::task::JoinSet::new();
    for (index, (source, resolved)) in sources.iter().cloned().enumerate() {
        let url = match resolved {
            Ok(url) => url,
            Err(reason) => {
                tasks.spawn(async move { (index, source, Err(reason)) });
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
            (index, source, result)
        });
    }

    let mut files: Vec<Option<DraftFile>> = vec![None; sources.len()];
    while let Some(joined) = tasks.join_next().await {
        if let Ok((index, source, probe)) = joined {
            files[index] = Some(DraftFile { source, probe });
        }
    }
    let files: Vec<DraftFile> = files.into_iter().flatten().collect();
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
        let name = match &group.archive {
            Some(archive) => archive.base_name.clone(),
            None => members
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
