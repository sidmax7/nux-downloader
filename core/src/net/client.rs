//! HTTP client construction (`ARCHITECTURE.md` §6.3). Every client this
//! crate makes — the unbound default one and, from Phase 3, every
//! link's own — shares the same transfer settings; only how (or
//! whether) a client binds to a network interface differs.

use crate::config::EngineConfig;
use crate::error::NetError;
use reqwest::header::{ACCEPT_ENCODING, HeaderMap, HeaderValue};
use reqwest::redirect::Policy;
use reqwest::{Client, ClientBuilder};

/// How many redirect hops a probe or transfer follows before giving up
/// (§6.3).
const MAX_REDIRECTS: usize = 5;

/// The settings every nuxd HTTP client shares:
/// - **HTTP/1.1 only.** Over HTTP/2, several "connections" to one host
///   would multiplex onto a single TCP connection — one congestion
///   window, one path — defeating per-connection parallelism.
/// - **No transparent decompression.** The `reqwest` dependency is built
///   without its `gzip`/`brotli`/`deflate`/`zstd` features (see
///   `core/Cargo.toml`), so this is true by construction; sending
///   `Accept-Encoding: identity` is a second, explicit line of defense
///   against a server that compresses regardless. Silent decompression
///   would break byte-range arithmetic.
/// - `rustls`, not the system TLS library, on every platform.
/// - A bounded redirect policy — `response.url()` then gives the final
///   URL a probe followed.
/// - A connect timeout, but deliberately **no** overall request timeout:
///   bodies are long-lived; stalls are caught per read instead (§8,
///   built in the driver).
/// - `tcp_nodelay(true)`.
///
/// Binding to a specific network link is layered on top of this builder
/// in Phase 3 (`net::bind`); this function has no link-specific
/// behavior at all.
/// Why a redirect must not be followed, if it mustn't: too many hops, or
/// a step from `https` down to plain `http`, which would let anyone on the
/// path swap the file.
pub(crate) fn redirect_refusal(
    hops_so_far: usize,
    from_scheme: &str,
    to_scheme: &str,
) -> Option<&'static str> {
    if hops_so_far >= MAX_REDIRECTS {
        Some("too many redirects")
    } else if from_scheme == "https" && to_scheme != "https" {
        Some("refusing a redirect from https down to http")
    } else {
        None
    }
}

fn redirect_policy() -> Policy {
    Policy::custom(|attempt| {
        let from = attempt
            .previous()
            .last()
            .map_or("", |previous| previous.scheme());
        match redirect_refusal(attempt.previous().len(), from, attempt.url().scheme()) {
            Some(reason) => attempt.error(reason),
            None => attempt.follow(),
        }
    })
}

pub(super) fn base_client_builder(config: &EngineConfig) -> ClientBuilder {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));

    Client::builder()
        .http1_only()
        .use_rustls_tls()
        .redirect(redirect_policy())
        .connect_timeout(config.connect_timeout)
        .tcp_nodelay(true)
        .default_headers(headers)
}

/// The shared, unbound client probes and resolvers use — whatever route
/// the OS picks (§6.3). A link's own client (`build_link_client`)
/// additionally binds to that link's interface, once Phase 3 builds
/// that.
pub fn build_default_client(config: &EngineConfig) -> Result<Client, NetError> {
    base_client_builder(config)
        .build()
        .map_err(|source| NetError::BuildClient { source })
}

/// A client for one link's transfer connections: the shared settings,
/// plus an idle pool sized to `slots_per_link` so that many connections
/// to one host are kept warm between blocks instead of reconnecting for
/// every new one. Not yet bound to any particular interface — that's
/// layered on top of this builder's output in Phase 3.
pub fn build_link_client(config: &EngineConfig) -> Result<Client, NetError> {
    base_client_builder(config)
        .pool_max_idle_per_host(usize::from(config.slots_per_link))
        .build()
        .map_err(|source| NetError::BuildClient { source })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_client_builds_successfully() {
        assert!(build_default_client(&EngineConfig::default()).is_ok());
    }

    #[test]
    fn link_client_builds_successfully() {
        assert!(build_link_client(&EngineConfig::default()).is_ok());
    }

    #[test]
    fn redirects_may_not_downgrade_from_https_or_loop_forever() {
        assert_eq!(redirect_refusal(0, "http", "http"), None);
        assert_eq!(redirect_refusal(1, "http", "https"), None);
        assert_eq!(redirect_refusal(2, "https", "https"), None);
        assert!(redirect_refusal(1, "https", "http").is_some());
        assert!(redirect_refusal(MAX_REDIRECTS, "http", "http").is_some());
    }
}
