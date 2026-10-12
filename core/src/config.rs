//! `EngineConfig` and `HostPolicy` — every default from `ARCHITECTURE.md`
//! §19, plus per-host connection limits (§7.1).

use std::time::Duration;

/// Per-host connection limits (§7.1): "Hosts cap connections per client
/// IP; each link is usually a different public IP."
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostLimits {
    pub max_connections_per_link: u32,
    /// `None` = unlimited.
    pub max_active_files: Option<u32>,
}

impl HostLimits {
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            max_connections_per_link: u32::MAX,
            max_active_files: None,
        }
    }
}

impl Default for HostLimits {
    fn default() -> Self {
        Self {
            max_connections_per_link: 8,
            max_active_files: None,
        }
    }
}

/// One glob rule: `*` matches any run of characters (including none),
/// anywhere in the hostname. `HostPolicy::limits_for` picks the most
/// specific matching rule — see its doc comment for what "specific"
/// means here.
#[derive(Debug, Clone, PartialEq)]
struct HostRule {
    pattern: String,
    limits: HostLimits,
}

/// Host limits come from `HostPolicy` rules (glob on hostname → limits),
/// with a default policy (§7.1).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HostPolicy {
    default: HostLimits,
    rules: Vec<HostRule>,
}

impl HostPolicy {
    #[must_use]
    pub fn new(default: HostLimits) -> Self {
        Self {
            default,
            rules: Vec::new(),
        }
    }

    /// Adds (or replaces) a rule for `pattern`. Later calls with the same
    /// pattern overwrite the earlier one rather than stacking.
    pub fn add_rule(&mut self, pattern: impl Into<String>, limits: HostLimits) -> &mut Self {
        let pattern = pattern.into();
        if let Some(existing) = self.rules.iter_mut().find(|rule| rule.pattern == pattern) {
            existing.limits = limits;
        } else {
            self.rules.push(HostRule { pattern, limits });
        }
        self
    }

    /// Adds a rule from `PATTERN=CONNS[:FILES]` text (the CLI's
    /// `--host-limit`): `CONNS` is the per-link connection cap and the
    /// optional `FILES` the cap on files active at once, e.g.
    /// `*.example.com=4:2`. Unspecified parts keep the defaults.
    pub fn add_rule_text(&mut self, text: &str) -> Result<&mut Self, String> {
        let (pattern, limits) = text
            .split_once('=')
            .ok_or_else(|| format!("expected PATTERN=CONNS[:FILES], got `{text}`"))?;
        if pattern.trim().is_empty() {
            return Err(format!("empty host pattern in `{text}`"));
        }
        let (conns, files) = match limits.split_once(':') {
            Some((conns, files)) => (conns, Some(files)),
            None => (limits, None),
        };
        let parse = |what: &str, value: &str| -> Result<u32, String> {
            match value.trim().parse::<u32>() {
                Ok(n) if n >= 1 => Ok(n),
                _ => Err(format!("{what} must be a whole number ≥ 1, got `{value}`")),
            }
        };
        let limits = HostLimits {
            max_connections_per_link: parse("connections", conns)?,
            max_active_files: files.map(|f| parse("files", f)).transpose()?,
        };
        Ok(self.add_rule(pattern.trim(), limits))
    }

    /// The limits that apply to `host`. When several rules match, the
    /// most specific one wins: specificity is the count of non-`*`
    /// characters in the pattern (a longer literal match beats a
    /// shorter one), and ties are broken by fewer `*` wildcards (an
    /// exact hostname always beats any pattern with a wildcard, since it
    /// has none). Falls back to the default policy when nothing matches.
    #[must_use]
    pub fn limits_for(&self, host: &str) -> HostLimits {
        self.rules
            .iter()
            .filter(|rule| glob_match(&rule.pattern, host))
            .max_by_key(|rule| specificity(&rule.pattern))
            .map_or(self.default, |rule| rule.limits)
    }
}

/// Non-wildcard character count, then negated wildcard count — a tuple
/// ordered so `Ord` picks the more specific pattern first (see
/// `limits_for`'s doc comment for the rule this implements).
fn specificity(pattern: &str) -> (usize, i64) {
    let wildcards = pattern.matches('*').count();
    let literal_chars = pattern.chars().count() - wildcards;
    (literal_chars, -(wildcards as i64))
}

/// `*` matches any run of characters, including an empty one. No other
/// wildcard syntax — hostnames don't need more than this.
///
/// Splits the pattern on `*` into segments: the first must prefix-match,
/// the last must suffix-match, and everything between must appear in
/// order somewhere in between. A pattern with no `*` must equal `text`
/// exactly.
fn glob_match(pattern: &str, text: &str) -> bool {
    let segments: Vec<&str> = pattern.split('*').collect();
    if segments.len() == 1 {
        return pattern == text;
    }

    let last = segments.len() - 1;
    let mut pos = 0usize;

    for (i, segment) in segments.iter().enumerate() {
        if segment.is_empty() {
            continue; // an empty piece between two `*`s, or a leading/trailing `*`
        }
        if i == last {
            return text[pos..].ends_with(segment);
        }
        let Some(found) = text[pos..].find(segment) else {
            return false;
        };
        if i == 0 && found != 0 {
            return false; // the leading segment must be a prefix, not just present
        }
        pos += found + segment.len();
    }
    true
}

/// Every tunable default from `ARCHITECTURE.md` §19.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineConfig {
    // --- Slots and constraints (§7.1) ---
    pub slots_per_link: u8,
    pub max_connections_total: u32,
    /// Probes (`Range: bytes=0-0` requests) in flight at once, so pasting
    /// hundreds of links doesn't open hundreds of connections (§5).
    pub max_concurrent_probes: u32,
    /// Probes in flight at once to one host.
    pub max_probes_per_host: u32,
    /// Extraction safety limits (§12.4). An archive that would write more
    /// than `max_extract_bytes` in total, more than `max_extract_ratio`
    /// times its own size (once past 1 GiB), or more than
    /// `max_extract_entries` entries is refused as a likely bomb.
    pub max_extract_bytes: u64,
    pub max_extract_ratio: u64,
    pub max_extract_entries: u64,
    pub soft_max_connections_per_file: u32,

    // --- Block planning (§7.4) ---
    pub single_block_threshold_bytes: u64,
    pub min_block_bytes: u64,
    pub max_block_bytes: u64,
    pub max_blocks: u32,
    pub block_alignment_bytes: u64,

    // --- Timeouts (§6.3, §8) ---
    pub stall_timeout: Duration,
    pub connect_timeout: Duration,

    // --- Retries (§7.6) ---
    pub retry_base_backoff: Duration,
    pub retry_max_backoff: Duration,
    pub max_retries_per_block: u32,

    // --- Link health (§6.4) ---
    pub link_degrade_after_failures: u32,
    pub link_degrade_backoff_min: Duration,
    pub link_degrade_backoff_max: Duration,

    // --- Tail handling (§7.5) ---
    pub hedge_after: Duration,
    pub max_hedges_per_block: u32,
    pub refresh_silent_after: Duration,
    /// Fraction (0.0–1.0) of a link's median speed below which a
    /// connection is considered crawling.
    pub refresh_slow_ratio: f64,
    pub refresh_slow_for: Duration,
    pub refresh_warmup: Duration,
    pub max_refreshes_per_block: u32,

    // --- Persistence (§9.3, §9.4) ---
    pub tick_interval: Duration,
    pub checkpoint_interval: Duration,
    pub checkpoint_bytes_threshold: u64,
    pub auto_resume_on_startup: bool,

    // --- Transfer (§8) ---
    pub write_buffer_bytes: usize,

    // --- UI and discovery ---
    pub progress_event_hz: u32,
    pub interface_rescan_interval: Duration,

    // --- Hosts (§7.1) ---
    pub hosts: HostPolicy,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            slots_per_link: 8,
            max_connections_total: 32,
            max_concurrent_probes: 8,
            max_probes_per_host: 2,
            max_extract_bytes: 512 * 1024 * 1024 * 1024,
            max_extract_ratio: 1000,
            max_extract_entries: 1_000_000,
            soft_max_connections_per_file: 8,

            single_block_threshold_bytes: 4 * 1024 * 1024,
            min_block_bytes: 1024 * 1024,
            max_block_bytes: 8 * 1024 * 1024,
            max_blocks: 4096,
            block_alignment_bytes: 64 * 1024,

            stall_timeout: Duration::from_secs(20),
            connect_timeout: Duration::from_secs(10),

            retry_base_backoff: Duration::from_secs(1),
            retry_max_backoff: Duration::from_secs(15),
            max_retries_per_block: 5,

            link_degrade_after_failures: 3,
            link_degrade_backoff_min: Duration::from_secs(5),
            link_degrade_backoff_max: Duration::from_secs(60),

            hedge_after: Duration::from_secs(8),
            max_hedges_per_block: 2,
            refresh_silent_after: Duration::from_secs(10),
            refresh_slow_ratio: 0.10,
            refresh_slow_for: Duration::from_secs(5),
            refresh_warmup: Duration::from_secs(3),
            max_refreshes_per_block: 2,

            tick_interval: Duration::from_millis(250),
            checkpoint_interval: Duration::from_secs(5),
            checkpoint_bytes_threshold: 64 * 1024 * 1024,
            auto_resume_on_startup: true,

            write_buffer_bytes: 256 * 1024,

            progress_event_hz: 4,
            interface_rescan_interval: Duration::from_secs(5),

            hosts: HostPolicy::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_text_sets_connections_and_optionally_files() {
        let mut policy = HostPolicy::default();
        policy.add_rule_text("*.example.com=4:2").unwrap();
        policy.add_rule_text("plain.org=3").unwrap();
        let a = policy.limits_for("cdn.example.com");
        assert_eq!(
            (a.max_connections_per_link, a.max_active_files),
            (4, Some(2))
        );
        let b = policy.limits_for("plain.org");
        assert_eq!((b.max_connections_per_link, b.max_active_files), (3, None));
        assert_eq!(policy.limits_for("other.net"), HostLimits::default());
    }

    #[test]
    fn rule_text_rejects_nonsense() {
        let mut policy = HostPolicy::default();
        for bad in ["nope", "=4", "a.com=0", "a.com=x", "a.com=2:0", "a.com=2:y"] {
            assert!(
                policy.add_rule_text(bad).is_err(),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn defaults_match_the_architecture_table() {
        let config = EngineConfig::default();
        assert_eq!(config.slots_per_link, 8);
        assert_eq!(config.max_connections_total, 32);
        assert_eq!(config.soft_max_connections_per_file, 8);
        assert_eq!(config.single_block_threshold_bytes, 4 * 1024 * 1024);
        assert_eq!(config.min_block_bytes, 1024 * 1024);
        assert_eq!(config.max_block_bytes, 8 * 1024 * 1024);
        assert_eq!(config.max_blocks, 4096);
        assert_eq!(config.block_alignment_bytes, 64 * 1024);
        assert_eq!(config.stall_timeout, Duration::from_secs(20));
        assert_eq!(config.connect_timeout, Duration::from_secs(10));
        assert_eq!(config.retry_base_backoff, Duration::from_secs(1));
        assert_eq!(config.retry_max_backoff, Duration::from_secs(15));
        assert_eq!(config.max_retries_per_block, 5);
        assert_eq!(config.link_degrade_after_failures, 3);
        assert_eq!(config.link_degrade_backoff_min, Duration::from_secs(5));
        assert_eq!(config.link_degrade_backoff_max, Duration::from_secs(60));
        assert_eq!(config.hedge_after, Duration::from_secs(8));
        assert_eq!(config.max_hedges_per_block, 2);
        assert_eq!(config.refresh_silent_after, Duration::from_secs(10));
        assert!((config.refresh_slow_ratio - 0.10).abs() < f64::EPSILON);
        assert_eq!(config.refresh_slow_for, Duration::from_secs(5));
        assert_eq!(config.refresh_warmup, Duration::from_secs(3));
        assert_eq!(config.max_refreshes_per_block, 2);
        assert_eq!(config.tick_interval, Duration::from_millis(250));
        assert_eq!(config.checkpoint_interval, Duration::from_secs(5));
        assert_eq!(config.checkpoint_bytes_threshold, 64 * 1024 * 1024);
        assert!(config.auto_resume_on_startup);
        assert_eq!(config.write_buffer_bytes, 256 * 1024);
        assert_eq!(config.progress_event_hz, 4);
        assert_eq!(config.interface_rescan_interval, Duration::from_secs(5));
    }

    #[test]
    fn default_host_policy_allows_eight_connections_unlimited_files() {
        let policy = HostPolicy::default();
        let limits = policy.limits_for("example.com");
        assert_eq!(limits.max_connections_per_link, 8);
        assert_eq!(limits.max_active_files, None);
    }

    #[test]
    fn unmatched_host_falls_back_to_default() {
        let mut policy = HostPolicy::new(HostLimits::default());
        policy.add_rule(
            "*.mediafire.com",
            HostLimits {
                max_connections_per_link: 4,
                max_active_files: Some(2),
            },
        );
        let limits = policy.limits_for("example.com");
        assert_eq!(limits.max_connections_per_link, 8);
    }

    #[test]
    fn wildcard_rule_matches_subdomains() {
        let mut policy = HostPolicy::new(HostLimits::default());
        policy.add_rule(
            "*.mediafire.com",
            HostLimits {
                max_connections_per_link: 4,
                max_active_files: Some(2),
            },
        );
        let limits = policy.limits_for("download1234.mediafire.com");
        assert_eq!(limits.max_connections_per_link, 4);
        assert_eq!(limits.max_active_files, Some(2));
    }

    #[test]
    fn exact_rule_beats_wildcard_rule() {
        let mut policy = HostPolicy::new(HostLimits::default());
        policy.add_rule(
            "*.example.com",
            HostLimits {
                max_connections_per_link: 4,
                max_active_files: None,
            },
        );
        policy.add_rule(
            "cdn.example.com",
            HostLimits {
                max_connections_per_link: 16,
                max_active_files: None,
            },
        );
        // The wildcard also matches "cdn.example.com" — the exact,
        // more-specific rule must win.
        let limits = policy.limits_for("cdn.example.com");
        assert_eq!(limits.max_connections_per_link, 16);
    }

    #[test]
    fn longer_literal_pattern_beats_shorter_one() {
        let mut policy = HostPolicy::new(HostLimits::default());
        policy.add_rule(
            "*.com",
            HostLimits {
                max_connections_per_link: 2,
                max_active_files: None,
            },
        );
        policy.add_rule(
            "*.example.com",
            HostLimits {
                max_connections_per_link: 16,
                max_active_files: None,
            },
        );
        let limits = policy.limits_for("host.example.com");
        assert_eq!(limits.max_connections_per_link, 16);
    }

    #[test]
    fn later_rule_for_same_pattern_replaces_earlier_one() {
        let mut policy = HostPolicy::new(HostLimits::default());
        policy.add_rule(
            "example.com",
            HostLimits {
                max_connections_per_link: 4,
                max_active_files: None,
            },
        );
        policy.add_rule(
            "example.com",
            HostLimits {
                max_connections_per_link: 12,
                max_active_files: None,
            },
        );
        assert_eq!(
            policy.limits_for("example.com").max_connections_per_link,
            12
        );
    }

    #[test]
    fn glob_match_handles_prefix_suffix_and_middle_wildcards() {
        assert!(glob_match("*.example.com", "cdn.example.com"));
        assert!(glob_match("cdn*.example.com", "cdn12.example.com"));
        assert!(glob_match("cdn*.example.com", "cdn.example.com"));
        assert!(glob_match("example.com", "example.com"));
        assert!(!glob_match("example.com", "other.com"));
        assert!(!glob_match("*.example.com", "example.com"));
        assert!(glob_match("*", "anything.at.all"));
    }
}
