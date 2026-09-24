//! The origin server itself: an axum app that serves deterministic
//! content (`content.rs`) and, per request, can be told to misbehave in
//! exactly the ways a real server or network does (`fault.rs`).

use crate::content::content_range;
use crate::fault::Fault;
use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{Notify, oneshot};

/// What every response otherwise looks like, before any per-request
/// fault is applied.
#[derive(Debug, Clone)]
pub struct OriginOptions {
    pub size: u64,
    pub seed: u64,
    /// `false`: ignore `Range` headers and never advertise range support
    /// (every response is a full `200`).
    pub ranges: bool,
    /// `false`: omit `Content-Length` on a full response, so the size is
    /// unknown to the client ahead of the transfer.
    pub content_length: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_disposition: Option<String>,
    /// Extra headers to add to every response — arbitrary, for cases
    /// none of the fields above cover (e.g. a `Content-Encoding: gzip`
    /// label to test that a client doesn't transparently decompress).
    pub extra_headers: Vec<(String, String)>,
    /// Where to listen. `127.0.0.1:0` (the default) picks any free port;
    /// a fixed loopback alias (`127.0.0.2`, …) is how Phase 3's
    /// multi-link tests get one "network" per origin-side source
    /// address without needing a real second interface.
    pub bind_addr: SocketAddr,
}

impl Default for OriginOptions {
    fn default() -> Self {
        Self {
            size: 1024,
            seed: 42,
            ranges: true,
            content_length: true,
            etag: Some("\"testkit-etag\"".to_string()),
            last_modified: None,
            content_disposition: None,
            extra_headers: Vec::new(),
            bind_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0),
        }
    }
}

/// A `Range: bytes=start-end` or `bytes=start-` request header, parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestedRange {
    pub start: u64,
    pub end: Option<u64>,
}

fn parse_range_header(value: &str) -> Option<RequestedRange> {
    let spec = value.strip_prefix("bytes=")?;
    let (start_str, end_str) = spec.split_once('-')?;
    let start: u64 = start_str.trim().parse().ok()?;
    let end = if end_str.trim().is_empty() {
        None
    } else {
        Some(end_str.trim().parse().ok()?)
    };
    Some(RequestedRange { start, end })
}

/// One request the origin has seen, with what it decided to do about it.
#[derive(Debug, Clone)]
pub struct LoggedRequest {
    /// 1-based count of requests this server has seen.
    pub n: usize,
    pub path: String,
    /// Source address — which "network" the request came in on, once
    /// several origin instances are bound to different loopback
    /// aliases (Phase 3).
    pub from: SocketAddr,
    pub range: Option<RequestedRange>,
    /// The request's `Accept-Encoding` header, verbatim, if it sent one.
    pub accept_encoding: Option<String>,
    pub fault: Fault,
    pub status: u16,
    /// Bytes of body actually written before the response ended, one way
    /// or another. Updated live as the body streams, not only at the end.
    pub bytes_sent: Arc<std::sync::atomic::AtomicU64>,
}

/// Locks `state`, turning a poisoned lock (a prior request's handler
/// panicked while holding it) into a loud panic here rather than a
/// silent `.unwrap()` the `unwrap_used` lint would otherwise catch —
/// same outcome, but as an explicit, named failure mode.
fn lock_state(state: &Arc<Mutex<SharedState>>) -> std::sync::MutexGuard<'_, SharedState> {
    state
        .lock()
        .unwrap_or_else(|poison| panic!("testkit: shared state lock poisoned: {poison}"))
}

struct SharedState {
    options: OriginOptions,
    requests: Vec<LoggedRequest>,
    fault_for_request: HashMap<usize, Fault>,
    default_fault: Fault,
    concurrent_by_source: HashMap<IpAddr, u32>,
    peak_concurrent_by_source: HashMap<IpAddr, u32>,
    /// Armed by `Origin::hold_next_request_at`: the next request's body
    /// pauses after this many bytes until `release()` is called, or
    /// forever if it never is (deterministic pause/kill tests).
    hold_next_at: Option<u64>,
}

/// Decrements the source's concurrency counter when the response body is
/// dropped — however that happens: finished, aborted, or the whole
/// server shutting down mid-stream.
struct ConcurrencyGuard {
    state: Arc<Mutex<SharedState>>,
    source: IpAddr,
}

impl Drop for ConcurrencyGuard {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock()
            && let Some(count) = state.concurrent_by_source.get_mut(&self.source)
        {
            *count = count.saturating_sub(1);
        }
    }
}

/// A running origin. Dropping it stops the server.
pub struct Origin {
    addr: SocketAddr,
    state: Arc<Mutex<SharedState>>,
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<tokio::task::JoinHandle<()>>,
    hold_notify: Arc<Notify>,
}

impl Origin {
    /// Starts listening immediately; the returned `Origin` is ready to
    /// take requests.
    pub async fn start(options: OriginOptions) -> Self {
        let listener = tokio::net::TcpListener::bind(options.bind_addr)
            .await
            .unwrap_or_else(|error| {
                panic!("testkit: couldn't bind {}: {error}", options.bind_addr)
            });
        let addr = listener.local_addr().unwrap_or(options.bind_addr);

        let state = Arc::new(Mutex::new(SharedState {
            options,
            requests: Vec::new(),
            fault_for_request: HashMap::new(),
            default_fault: Fault::Ok,
            concurrent_by_source: HashMap::new(),
            peak_concurrent_by_source: HashMap::new(),
            hold_next_at: None,
        }));
        let hold_notify = Arc::new(Notify::new());

        let app = Router::new()
            .fallback(get(handle_request))
            .with_state(AppState {
                shared: state.clone(),
                hold_notify: hold_notify.clone(),
            });

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let join = tokio::spawn(serve(listener, app, shutdown_rx));

        Self {
            addr,
            state,
            shutdown: Some(shutdown_tx),
            join: Some(join),
            hold_notify,
        }
    }

    /// The base URL for the file this origin serves.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}/file", self.addr)
    }

    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Every request handled so far, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<LoggedRequest> {
        lock_state(&self.state).requests.clone()
    }

    /// The fault to apply to the `n`th request (1-based). Overrides the
    /// default fault for that one request only.
    pub fn set_fault(&self, n: usize, fault: Fault) {
        lock_state(&self.state).fault_for_request.insert(n, fault);
    }

    /// The fault every request gets unless overridden by `set_fault`.
    pub fn set_default_fault(&self, fault: Fault) {
        lock_state(&self.state).default_fault = fault;
    }

    /// The highest number of requests from one source address that were
    /// in flight at once — for asserting a per-link connection cap was
    /// respected (Phase 3+).
    #[must_use]
    pub fn peak_concurrent_from(&self, source: IpAddr) -> u32 {
        lock_state(&self.state)
            .peak_concurrent_by_source
            .get(&source)
            .copied()
            .unwrap_or(0)
    }

    /// Arms a one-shot hold: the next request's body pauses after
    /// `offset` bytes until `release()` is called. Lets a test get a
    /// transfer into an exact, known state before pausing or killing
    /// the client, instead of racing on timing.
    pub fn hold_next_request_at(&self, offset: u64) {
        lock_state(&self.state).hold_next_at = Some(offset);
    }

    /// Releases every request currently paused by a hold.
    pub fn release(&self) {
        self.hold_notify.notify_waiters();
    }

    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
    }
}

impl Drop for Origin {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

async fn serve(listener: TcpListener, app: Router, shutdown_rx: oneshot::Receiver<()>) {
    let make_service = app.into_make_service_with_connect_info::<SocketAddr>();
    let server = axum::serve(listener, make_service).with_graceful_shutdown(async {
        let _ = shutdown_rx.await;
    });
    let _ = server.await;
}

#[derive(Clone)]
struct AppState {
    shared: Arc<Mutex<SharedState>>,
    hold_notify: Arc<Notify>,
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_string)
}

async fn handle_request(
    State(app): State<AppState>,
    ConnectInfo(from): ConnectInfo<SocketAddr>,
    request: Request,
) -> Response {
    let path = request.uri().path().to_string();
    let range = header_str(request.headers(), header::RANGE)
        .as_deref()
        .and_then(parse_range_header);
    let accept_encoding = header_str(request.headers(), header::ACCEPT_ENCODING);

    let (options, fault, n, hold_at) = {
        let mut state = lock_state(&app.shared);
        let n = state.requests.len() + 1;
        let fault = state
            .fault_for_request
            .remove(&n)
            .unwrap_or_else(|| state.default_fault.clone());
        let hold_at = state.hold_next_at.take();

        let count = *state
            .concurrent_by_source
            .entry(from.ip())
            .and_modify(|c| *c += 1)
            .or_insert(1);
        let peak = state
            .peak_concurrent_by_source
            .entry(from.ip())
            .or_insert(0);
        *peak = (*peak).max(count);

        (state.options.clone(), fault, n, hold_at)
    };

    let bytes_sent = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let guard = ConcurrencyGuard {
        state: app.shared.clone(),
        source: from.ip(),
    };

    let (status, headers, body) = build_response(
        &options,
        range,
        &fault,
        hold_at,
        app.hold_notify.clone(),
        bytes_sent.clone(),
        guard,
    )
    .await;

    {
        let mut state = lock_state(&app.shared);
        state.requests.push(LoggedRequest {
            n,
            path,
            from,
            range,
            accept_encoding,
            fault,
            status: status.as_u16(),
            bytes_sent,
        });
    }

    let mut response = Response::builder().status(status);
    for (name, value) in headers {
        response = response.header(name, value);
    }
    response
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

type Headers = Vec<(header::HeaderName, HeaderValue)>;

/// Decides status, headers and body for one request. `_guard` is only
/// here to be moved into the body stream below, so the concurrency
/// counter stays incremented for as long as the body is still being
/// sent.
async fn build_response(
    options: &OriginOptions,
    range: Option<RequestedRange>,
    fault: &Fault,
    hold_at: Option<u64>,
    hold_notify: Arc<Notify>,
    bytes_sent: Arc<std::sync::atomic::AtomicU64>,
    guard: ConcurrencyGuard,
) -> (StatusCode, Headers, Body) {
    match fault {
        Fault::StallHeaders => {
            std::future::pending::<()>().await;
            unreachable!("testkit: StallHeaders never resolves")
        }
        Fault::Status(code) => {
            let status = StatusCode::from_u16(*code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Vec::new(), Body::empty())
        }
        Fault::Redirect(location) => {
            let headers = HeaderValue::from_str(location)
                .map(|value| vec![(header::LOCATION, value)])
                .unwrap_or_default();
            (StatusCode::FOUND, headers, Body::empty())
        }
        _ => build_content_response(
            options,
            range,
            fault,
            hold_at,
            hold_notify,
            bytes_sent,
            guard,
        ),
    }
}

/// The shared path for every fault that still serves file content, just
/// with something wrong about the headers or the body stream.
fn build_content_response(
    options: &OriginOptions,
    range: Option<RequestedRange>,
    fault: &Fault,
    hold_at: Option<u64>,
    hold_notify: Arc<Notify>,
    bytes_sent: Arc<std::sync::atomic::AtomicU64>,
    guard: ConcurrencyGuard,
) -> (StatusCode, Headers, Body) {
    let effective_range = if matches!(fault, Fault::IgnoreRange) {
        None
    } else {
        range
    };

    let mut headers: Headers = Vec::new();
    if let Some(etag) = &options.etag
        && let Ok(value) = HeaderValue::from_str(etag)
    {
        headers.push((header::ETAG, value));
    }
    if let Some(last_modified) = &options.last_modified
        && let Ok(value) = HeaderValue::from_str(last_modified)
    {
        headers.push((header::LAST_MODIFIED, value));
    }
    if let Some(disposition) = &options.content_disposition
        && let Ok(value) = HeaderValue::from_str(disposition)
    {
        headers.push((header::CONTENT_DISPOSITION, value));
    }
    for (name, value) in &options.extra_headers {
        if let Ok(header_name) = header::HeaderName::from_bytes(name.as_bytes())
            && let Ok(header_value) = HeaderValue::from_str(value)
        {
            headers.push((header_name, header_value));
        }
    }

    let (status, serve_start, serve_len, content_range_header) = match effective_range {
        Some(requested) if options.ranges => {
            let start = requested.start;
            if start >= options.size {
                let header_value = format!("bytes */{}", options.size);
                return (
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    {
                        let mut h = headers;
                        if let Ok(value) = HeaderValue::from_str(&header_value) {
                            h.push((header::CONTENT_RANGE, value));
                        }
                        h
                    },
                    Body::empty(),
                );
            }
            let end = requested
                .end
                .unwrap_or(options.size - 1)
                .min(options.size - 1);
            let len = end - start + 1;

            let advertised_start = if *fault == Fault::WrongStart {
                start + 1
            } else {
                start
            };
            let content_range = format!("bytes {advertised_start}-{end}/{}", options.size);
            (StatusCode::PARTIAL_CONTENT, start, len, Some(content_range))
        }
        _ => (StatusCode::OK, 0, options.size, None),
    };

    if let Some(value) = content_range_header.filter(|_| *fault != Fault::NoContentRange)
        && let Ok(header_value) = HeaderValue::from_str(&value)
    {
        headers.push((header::CONTENT_RANGE, header_value));
    }

    // Extra body byte for Overlong; how many bytes the stream actually
    // sends before cutting off / ending short / hanging, for the
    // transport-level faults.
    let overlong_extra = u64::from(*fault == Fault::Overlong);
    let full_len = serve_len + overlong_extra;

    let is_transport_fault = matches!(
        fault,
        Fault::CutAfter(_) | Fault::EndAfter(_) | Fault::StallBody | Fault::Crawl(_)
    );
    if options.content_length
        && !is_transport_fault
        && let Ok(value) = HeaderValue::from_str(&full_len.to_string())
    {
        headers.push((header::CONTENT_LENGTH, value));
    }

    let seed = options.seed;
    let fault = fault.clone();
    let stream = async_stream::stream! {
        let _guard = guard;
        let mut sent = 0u64;
        let hold_point = hold_at;

        // Caps a chunk so it never crosses the hold offset — otherwise a
        // pause armed for byte 100 could sit inside one big 64 KiB chunk
        // and never actually take effect at that exact byte.
        macro_rules! clip_to_hold {
            ($natural:expr) => {{
                let natural = $natural;
                match hold_point {
                    Some(hold) if sent < hold && hold < sent + natural => hold - sent,
                    _ => natural,
                }
            }};
        }
        macro_rules! maybe_hold {
            () => {
                if hold_point == Some(sent) {
                    hold_notify.notified().await;
                }
            };
        }

        match &fault {
            Fault::CutAfter(n) => {
                let cap = (*n).min(full_len);
                while sent < cap {
                    let chunk_len = clip_to_hold!((cap - sent).min(4096));
                    let bytes = Bytes::from(content_range(seed, serve_start + sent, chunk_len));
                    sent += chunk_len;
                    bytes_sent.store(sent, std::sync::atomic::Ordering::Relaxed);
                    yield Ok(bytes);
                    maybe_hold!();
                }
                yield Err(io::Error::other("testkit: connection cut by fault injection"));
            }
            Fault::EndAfter(n) => {
                let cap = (*n).min(full_len);
                while sent < cap {
                    let chunk_len = clip_to_hold!((cap - sent).min(4096));
                    let bytes = Bytes::from(content_range(seed, serve_start + sent, chunk_len));
                    sent += chunk_len;
                    bytes_sent.store(sent, std::sync::atomic::Ordering::Relaxed);
                    yield Ok(bytes);
                    maybe_hold!();
                }
                // Ends the stream cleanly with fewer bytes than any
                // Content-Range promised — a short body, not an error.
            }
            Fault::StallBody => {
                std::future::pending::<()>().await;
            }
            Fault::Crawl(rate) => {
                let rate = (*rate).max(1);
                let chunk = (rate / 10).clamp(1, 4096); // ~10 pieces/sec
                let delay = Duration::from_millis(100);
                while sent < full_len {
                    let chunk_len = clip_to_hold!(chunk.min(full_len - sent));
                    let bytes = Bytes::from(content_range(seed, serve_start + sent, chunk_len));
                    sent += chunk_len;
                    bytes_sent.store(sent, std::sync::atomic::Ordering::Relaxed);
                    yield Ok(bytes);
                    maybe_hold!();
                    tokio::time::sleep(delay).await;
                }
            }
            _ => {
                while sent < full_len {
                    let chunk_len = clip_to_hold!((full_len - sent).min(64 * 1024));
                    let bytes = Bytes::from(content_range(seed, serve_start + sent, chunk_len));
                    sent += chunk_len;
                    bytes_sent.store(sent, std::sync::atomic::Ordering::Relaxed);
                    yield Ok(bytes);
                    maybe_hold!();
                }
            }
        }
    };

    (status, headers, Body::from_stream(stream))
}
