//! Byte ranges and rate limits.

use serde::{Deserialize, Serialize};

/// An inclusive byte range into a file. `end == None` means "to the end
/// of the file" — used for an unknown-size file's single open-ended
/// block (`ARCHITECTURE.md` §7.4), mirroring the `Range: bytes=N-` HTTP
/// header that has no upper bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ByteRange {
    pub start: u64,
    pub end: Option<u64>,
}

impl ByteRange {
    /// A bounded range `[start, end]` inclusive. Returns `None` if
    /// `end < start` — constructing a range that way is always a bug, not
    /// a runtime condition to propagate.
    #[must_use]
    pub const fn bounded(start: u64, end: u64) -> Option<Self> {
        if end < start {
            return None;
        }
        Some(Self {
            start,
            end: Some(end),
        })
    }

    /// A range from `start` to the end of the file, whatever that turns
    /// out to be.
    #[must_use]
    pub const fn open_ended(start: u64) -> Self {
        Self { start, end: None }
    }

    /// The range's length in bytes, if it's bounded. `bounded()` is the
    /// only way to construct an inverted range's rejection, but the
    /// fields are public (matching the rest of the domain model), so
    /// this stays defensive rather than trusting that invariant: an
    /// `end` before `start` reads as an empty range instead of
    /// underflowing.
    #[must_use]
    pub const fn len(&self) -> Option<u64> {
        match self.end {
            Some(end) if end >= self.start => Some(end - self.start + 1),
            Some(_) => Some(0),
            None => None,
        }
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        matches!(self.len(), Some(0))
    }
}

/// A rate limit in bytes per second. Always positive — "no limit" is
/// expressed as `Option<ByteRate> = None` at the call site (`Package`,
/// `Link`), not as a `ByteRate` of zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ByteRate(u64);

impl ByteRate {
    /// Returns `None` for `0`, which is not a meaningful rate limit — the
    /// caller almost always meant "no limit" (`Option<ByteRate> = None`),
    /// and a zero-rate limiter would simply hang forever.
    #[must_use]
    pub const fn new(bytes_per_sec: u64) -> Option<Self> {
        if bytes_per_sec == 0 {
            return None;
        }
        Some(Self(bytes_per_sec))
    }

    #[must_use]
    pub const fn bytes_per_sec(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_rejects_end_before_start() {
        assert_eq!(ByteRange::bounded(10, 5), None);
    }

    #[test]
    fn bounded_accepts_a_single_byte_range() {
        let range = ByteRange::bounded(5, 5).unwrap();
        assert_eq!(range.len(), Some(1));
        assert!(!range.is_empty());
    }

    #[test]
    fn len_partitions_correctly() {
        let range = ByteRange::bounded(0, 99).unwrap();
        assert_eq!(range.len(), Some(100));
    }

    #[test]
    fn open_ended_has_no_length() {
        assert_eq!(ByteRange::open_ended(0).len(), None);
    }

    #[test]
    fn zero_is_not_a_valid_rate() {
        assert_eq!(ByteRate::new(0), None);
    }

    #[test]
    fn positive_rate_round_trips() {
        let rate = ByteRate::new(1024).unwrap();
        assert_eq!(rate.bytes_per_sec(), 1024);
    }

    #[test]
    fn serde_round_trip() {
        let rate = ByteRate::new(5_000_000).unwrap();
        let json = serde_json::to_string(&rate).unwrap();
        assert_eq!(json, "5000000");
        let back: ByteRate = serde_json::from_str(&json).unwrap();
        assert_eq!(back, rate);
    }
}
