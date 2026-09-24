//! What the origin does with one request. Named after the specific,
//! narrow way each one misbehaves, so a test asserting "the client
//! recovers from X" names exactly what X is.

/// Anything other than `Ok` is a fault. Mirrors the fault set an idea
/// from Plexo's e2e origin (`e2e/origin.ts`) — translated, not ported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// Serve the request correctly.
    Ok,
    /// Reply `200` with the whole file, as a server without range
    /// support does — even though `Range` was sent.
    IgnoreRange,
    /// `206` whose `Content-Range` starts one byte later than asked.
    WrongStart,
    /// `206` with a correct header but one byte more body than the
    /// range promised.
    Overlong,
    /// `206` with no `Content-Range` header at all.
    NoContentRange,
    /// Never answer — no headers, no body, ever.
    StallHeaders,
    /// Send headers, then nothing, forever.
    StallBody,
    /// Send this many body bytes, then abort the connection.
    CutAfter(u64),
    /// Send this many body bytes, then end the response cleanly (a
    /// short body — fewer bytes than `Content-Length` promised).
    EndAfter(u64),
    /// Trickle the body at this rate (bytes/sec) in small pieces — slow,
    /// but never silent long enough on its own to look stalled.
    Crawl(u64),
    /// Serve the correct body at up to this many bytes/sec — a link of a
    /// given speed, for multi-link tests. Unlike `Crawl` (which stays
    /// under ~40 KB/s so a stall test can outlast a timeout) this can
    /// model megabytes per second.
    Throttle(u64),
    /// `429 Too Many Requests` with `Retry-After: <seconds>`, as a
    /// rate-limiting server sends.
    RateLimited(u64),
    /// Wait this many milliseconds before answering, then serve the
    /// request correctly — a slow server, for probe-pool and overlap
    /// tests.
    Delay(u64),
    /// Reply with this status and no body.
    Status(u16),
    /// Redirect to this URL (a `302`).
    Redirect(String),
}
