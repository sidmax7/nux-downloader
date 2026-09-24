//! Link resolution (`ARCHITECTURE.md` §13): turns what a user pastes into
//! direct URLs. A `Registry` tries resolvers in order; v1 ships only
//! `DirectUrl`, which validates `http`/`https` and does no network I/O of
//! its own — redirects and short links are followed by the probe, which
//! records the final URL (`ProbeResult::final_url`), and re-resolving an
//! expired URL (§13: on `403`/`410`) is a re-probe of the file's source.
//!
//! The scope guardrails in `CLAUDE.md` apply to every resolver: nothing
//! here may bypass captchas or anti-bot checks, circumvent DRM or stream
//! protection, or handle premium-account credentials.

use crate::error::ResolveError;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use url::Url;

/// What a resolver may use: the unbound HTTP client and a way to be
/// cancelled.
#[derive(Clone)]
pub struct ResolveContext {
    pub client: reqwest::Client,
    pub cancel: CancellationToken,
}

/// One direct URL a resolver produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLink {
    pub url: Url,
}

/// Turns one input into zero or more direct URLs.
#[async_trait::async_trait]
pub trait LinkResolver: Send + Sync {
    /// Whether this resolver wants `input`.
    fn matches(&self, input: &str) -> bool;
    async fn resolve(
        &self,
        input: &str,
        ctx: &ResolveContext,
    ) -> Result<Vec<ResolvedLink>, ResolveError>;
}

/// Validates a direct `http`/`https` URL and returns it as is.
#[derive(Debug, Clone, Copy, Default)]
pub struct DirectUrl;

/// Whether `url` is a scheme this tool downloads.
#[must_use]
pub fn is_supported_scheme(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
}

#[async_trait::async_trait]
impl LinkResolver for DirectUrl {
    fn matches(&self, _input: &str) -> bool {
        true // the fallback: anything a more specific resolver skipped
    }

    async fn resolve(
        &self,
        input: &str,
        _ctx: &ResolveContext,
    ) -> Result<Vec<ResolvedLink>, ResolveError> {
        let trimmed = input.trim();
        let url = Url::parse(trimmed).map_err(|_| ResolveError::InvalidUrl {
            input: trimmed.to_string(),
        })?;
        if !is_supported_scheme(&url) || url.host_str().is_none() {
            return Err(ResolveError::UnsupportedScheme {
                input: trimmed.to_string(),
                scheme: url.scheme().to_string(),
            });
        }
        Ok(vec![ResolvedLink { url }])
    }
}

/// The resolvers, tried in registration order; the first whose `matches`
/// accepts an input handles it.
#[derive(Clone)]
pub struct Registry {
    resolvers: Vec<Arc<dyn LinkResolver>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new().with(Arc::new(DirectUrl))
    }
}

impl Registry {
    /// An empty registry — `resolve` fails for everything until
    /// something is registered. `Registry::default()` has `DirectUrl`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            resolvers: Vec::new(),
        }
    }

    #[must_use]
    pub fn with(mut self, resolver: Arc<dyn LinkResolver>) -> Self {
        self.resolvers.push(resolver);
        self
    }

    pub async fn resolve(
        &self,
        input: &str,
        ctx: &ResolveContext,
    ) -> Result<Vec<ResolvedLink>, ResolveError> {
        let trimmed = input.trim();
        match self.resolvers.iter().find(|r| r.matches(trimmed)) {
            Some(resolver) => resolver.resolve(trimmed, ctx).await,
            None => Err(ResolveError::NoResolver {
                input: trimmed.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ResolveContext {
        ResolveContext {
            client: reqwest::Client::new(),
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn direct_url_accepts_http_and_https() {
        let registry = Registry::default();
        for input in ["http://example.com/a.bin", " https://example.com/b.bin\n"] {
            let links = registry.resolve(input, &ctx()).await.unwrap();
            assert_eq!(links.len(), 1);
            assert_eq!(links[0].url.as_str(), input.trim());
        }
    }

    #[tokio::test]
    async fn other_schemes_are_rejected() {
        let registry = Registry::default();
        for (input, scheme) in [
            ("ftp://example.com/a.bin", "ftp"),
            ("file:///etc/passwd", "file"),
            ("magnet:?xt=urn:btih:abc", "magnet"),
            ("javascript:alert(1)", "javascript"),
        ] {
            match registry.resolve(input, &ctx()).await {
                Err(ResolveError::UnsupportedScheme { scheme: s, .. }) => assert_eq!(s, scheme),
                other => panic!("{input}: expected UnsupportedScheme, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn garbage_is_an_invalid_url() {
        let registry = Registry::default();
        assert!(matches!(
            registry.resolve("not a url", &ctx()).await,
            Err(ResolveError::InvalidUrl { .. })
        ));
    }

    #[tokio::test]
    async fn an_empty_registry_resolves_nothing() {
        assert!(matches!(
            Registry::new().resolve("http://example.com/", &ctx()).await,
            Err(ResolveError::NoResolver { .. })
        ));
    }

    /// A stand-in for a future resolver: claims one host and expands it.
    struct Fanout;
    #[async_trait::async_trait]
    impl LinkResolver for Fanout {
        fn matches(&self, input: &str) -> bool {
            input.starts_with("http://album.example/")
        }
        async fn resolve(
            &self,
            _input: &str,
            _ctx: &ResolveContext,
        ) -> Result<Vec<ResolvedLink>, ResolveError> {
            Ok((1..=3)
                .map(|n| ResolvedLink {
                    url: Url::parse(&format!("http://cdn.example/{n}.jpg")).unwrap(),
                })
                .collect())
        }
    }

    #[tokio::test]
    async fn the_first_matching_resolver_wins_and_may_return_many() {
        let registry = Registry::new()
            .with(Arc::new(Fanout))
            .with(Arc::new(DirectUrl));
        let many = registry
            .resolve("http://album.example/x", &ctx())
            .await
            .unwrap();
        assert_eq!(many.len(), 3);
        let one = registry
            .resolve("http://plain.example/y", &ctx())
            .await
            .unwrap();
        assert_eq!(one.len(), 1);
    }
}
