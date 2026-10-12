//! Small text parsers: checksum sidecars, rates, URL redaction, host rules.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuxd_core::config::HostPolicy;
use nuxd_core::diagnostics::redact_urls;
use nuxd_core::model::ByteRate;
use nuxd_core::verify::{HashKind, parse_sidecar};

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    for kind in [HashKind::Crc32, HashKind::Md5, HashKind::Sha256] {
        for entry in parse_sidecar(kind, &text) {
            assert!(!entry.name.is_empty());
            assert!(!entry.name.contains('/') && !entry.name.contains('\\'));
            assert!(entry.hash.bytes().all(|b| b.is_ascii_hexdigit()));
        }
    }
    let _ = ByteRate::parse(&text);
    let _ = HostPolicy::default().add_rule_text(&text);

    let redacted = redact_urls(&text);
    // Whatever follows a `?` inside a URL is gone.
    for scheme in ["http://", "https://"] {
        for url in redacted.split(scheme).skip(1) {
            let token = url
                .split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')' | ']'))
                .next()
                .unwrap_or("");
            assert!(!token.contains('?') || token.contains('…') || token.starts_with('?'),
                    "query survived: {token:?}");
        }
    }
});
