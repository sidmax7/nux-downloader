//! Detecting a changed remote file (`ARCHITECTURE.md` §10). Inspired by
//! Plexo's `fileVersion.ts`: a different size is proof the file changed;
//! a different `ETag` or `Last-Modified` is only a hint, since a
//! load-balanced server can label identical bytes differently.

use crate::model::FileVersion;

/// How a response's version compares with the ones a download has
/// accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionCheck {
    /// Nothing suggests a different file.
    Same,
    /// The size differs. Identical bytes can't have a different size, so
    /// this is proof.
    Size { detail: String },
    /// The `ETag` or `Last-Modified` differs. That's only a hint — it has
    /// to be settled by comparing actual bytes (not this module's job;
    /// see `ARCHITECTURE.md` §10's byte-sampling step, built in Phase 2).
    Validators { detail: String },
}

/// Strips the leading `W/` weak-validator marker, if present
/// (case-insensitively, matching RFC 7232's `W/` — servers occasionally
/// lower-case it).
fn strip_weak_marker(s: &str) -> &str {
    let mut chars = s.char_indices();
    match (chars.next(), chars.next()) {
        (Some((_, first)), Some((second_index, '/'))) if first == 'W' || first == 'w' => {
            &s[second_index + 1..]
        }
        _ => s,
    }
}

/// Strips a pair of surrounding double quotes, but only when they
/// bracket the *entire* remaining string (matching `^"(.*)"$`) — a lone
/// quote character, or one only at the start, is left alone.
fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Strips a trailing `-gzip` / `;gzip` (or `br` / `deflate` / `zstd`)
/// suffix some servers append for a compressed variant of otherwise
/// identical content (e.g. Apache's `mod_deflate`).
fn strip_compression_suffix(s: &str) -> &str {
    const WORDS: [&str; 4] = ["gzip", "br", "deflate", "zstd"];
    for word in WORDS {
        for separator in ['-', ';'] {
            let needle_len = separator.len_utf8() + word.len();
            if s.len() < needle_len {
                continue;
            }
            let tail = &s[s.len() - needle_len..];
            if tail.starts_with(separator)
                && tail[separator.len_utf8()..].eq_ignore_ascii_case(word)
            {
                return &s[..s.len() - needle_len];
            }
        }
    }
    s
}

/// Drops what varies between servers without the bytes changing: the
/// weak-validator marker, the quotes, and a compressed-variant suffix.
#[must_use]
pub fn normalize_etag(etag: &str) -> String {
    let s = etag.trim();
    let s = strip_weak_marker(s);
    let s = strip_quotes(s);
    let s = strip_compression_suffix(s);
    s.to_string()
}

/// `accepted` is the version a download started on, plus any since
/// confirmed to serve identical bytes (§10's byte-sampling step appends
/// to this list; that step is not this function's job).
#[must_use]
pub fn compare(accepted: &[FileVersion], seen: &FileVersion) -> VersionCheck {
    let expected_size = accepted.first().map_or(0, |version| version.total_bytes);
    if expected_size > 0 && seen.total_bytes > 0 && seen.total_bytes != expected_size {
        return VersionCheck::Size {
            detail: format!(
                "it is now {} bytes instead of {expected_size}",
                seen.total_bytes
            ),
        };
    }

    let etags: Vec<String> = accepted
        .iter()
        .filter_map(|version| version.etag.as_deref().map(normalize_etag))
        .collect();
    if let Some(seen_etag) = &seen.etag
        && !etags.is_empty()
    {
        return if etags.contains(&normalize_etag(seen_etag)) {
            VersionCheck::Same
        } else {
            VersionCheck::Validators {
                detail: format!("its ETag is now {seen_etag}"),
            }
        };
    }

    let dates: Vec<&str> = accepted
        .iter()
        .filter_map(|version| version.last_modified.as_deref())
        .collect();
    if let Some(seen_date) = &seen.last_modified
        && !dates.is_empty()
        && !dates.contains(&seen_date.as_str())
    {
        return VersionCheck::Validators {
            detail: format!("it was modified at {seen_date}"),
        };
    }

    VersionCheck::Same
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(etag: Option<&str>, last_modified: Option<&str>, total_bytes: u64) -> FileVersion {
        FileVersion {
            etag: etag.map(String::from),
            last_modified: last_modified.map(String::from),
            total_bytes,
        }
    }

    // --- normalize_etag ---

    #[test]
    fn normalize_strips_surrounding_quotes() {
        assert_eq!(normalize_etag("\"abc123\""), "abc123");
    }

    #[test]
    fn normalize_strips_weak_marker_and_quotes() {
        assert_eq!(normalize_etag("W/\"abc123\""), "abc123");
        assert_eq!(normalize_etag("w/\"abc123\""), "abc123");
    }

    #[test]
    fn normalize_strips_compression_suffix() {
        assert_eq!(normalize_etag("\"abc123-gzip\""), "abc123");
        assert_eq!(normalize_etag("\"abc123;br\""), "abc123");
        assert_eq!(normalize_etag("\"abc123-DEFLATE\""), "abc123");
    }

    #[test]
    fn normalize_trims_whitespace() {
        assert_eq!(normalize_etag("  \"abc123\"  "), "abc123");
    }

    #[test]
    fn normalize_leaves_a_bare_token_alone() {
        assert_eq!(normalize_etag("abc123"), "abc123");
    }

    #[test]
    fn normalize_leaves_a_lone_quote_alone() {
        assert_eq!(normalize_etag("\""), "\"");
    }

    // --- compare ---

    #[test]
    fn identical_etag_is_same() {
        let accepted = [version(Some("\"abc\""), None, 100)];
        let seen = version(Some("\"abc\""), None, 100);
        assert_eq!(compare(&accepted, &seen), VersionCheck::Same);
    }

    #[test]
    fn different_size_is_proof_of_change() {
        let accepted = [version(Some("\"abc\""), None, 100)];
        let seen = version(Some("\"abc\""), None, 200);
        assert_eq!(
            compare(&accepted, &seen),
            VersionCheck::Size {
                detail: "it is now 200 bytes instead of 100".into()
            }
        );
    }

    #[test]
    fn zero_total_bytes_on_either_side_skips_the_size_check() {
        // `0` means "unknown" (`FileVersion::total_bytes` doc comment) —
        // it must never be treated as a genuine size mismatch.
        let accepted = [version(Some("\"abc\""), None, 0)];
        let seen = version(Some("\"abc\""), None, 200);
        assert_eq!(compare(&accepted, &seen), VersionCheck::Same);

        let accepted = [version(Some("\"abc\""), None, 100)];
        let seen = version(Some("\"abc\""), None, 0);
        assert_eq!(compare(&accepted, &seen), VersionCheck::Same);
    }

    #[test]
    fn same_size_different_etag_is_a_hint_not_proof() {
        let accepted = [version(Some("\"abc\""), None, 100)];
        let seen = version(Some("\"xyz\""), None, 100);
        assert_eq!(
            compare(&accepted, &seen),
            VersionCheck::Validators {
                detail: "its ETag is now \"xyz\"".into()
            }
        );
    }

    #[test]
    fn weak_and_strong_forms_of_the_same_etag_are_same() {
        let accepted = [version(Some("\"abc\""), None, 100)];
        let seen = version(Some("W/\"abc\""), None, 100);
        assert_eq!(compare(&accepted, &seen), VersionCheck::Same);
    }

    #[test]
    fn compression_suffixed_etag_matches_its_plain_form() {
        let accepted = [version(Some("\"abc\""), None, 100)];
        let seen = version(Some("\"abc-gzip\""), None, 100);
        assert_eq!(compare(&accepted, &seen), VersionCheck::Same);
    }

    #[test]
    fn matching_any_accepted_version_is_enough() {
        // Simulates the confirm-same-bytes flow (§10): a second version
        // was appended after being confirmed identical, and a later
        // response matching *either* accepted label is accepted.
        let accepted = [
            version(Some("\"abc\""), None, 100),
            version(Some("\"def\""), None, 100),
        ];
        let seen = version(Some("\"def\""), None, 100);
        assert_eq!(compare(&accepted, &seen), VersionCheck::Same);
    }

    #[test]
    fn last_modified_change_is_a_hint_when_no_etag_is_available() {
        let accepted = [version(None, Some("Mon, 01 Jan 2024 00:00:00 GMT"), 100)];
        let seen = version(None, Some("Tue, 02 Jan 2024 00:00:00 GMT"), 100);
        assert_eq!(
            compare(&accepted, &seen),
            VersionCheck::Validators {
                detail: "it was modified at Tue, 02 Jan 2024 00:00:00 GMT".into()
            }
        );
    }

    #[test]
    fn matching_last_modified_is_same() {
        let accepted = [version(None, Some("Mon, 01 Jan 2024 00:00:00 GMT"), 100)];
        let seen = version(None, Some("Mon, 01 Jan 2024 00:00:00 GMT"), 100);
        assert_eq!(compare(&accepted, &seen), VersionCheck::Same);
    }

    #[test]
    fn missing_validators_on_both_sides_is_same() {
        let accepted = [version(None, None, 100)];
        let seen = version(None, None, 100);
        assert_eq!(compare(&accepted, &seen), VersionCheck::Same);
    }

    #[test]
    fn a_newly_present_etag_with_none_accepted_is_not_a_mismatch() {
        // Nothing to compare against yet — a server that starts sending
        // an ETag it previously omitted isn't evidence of anything.
        let accepted = [version(None, None, 100)];
        let seen = version(Some("\"abc\""), None, 100);
        assert_eq!(compare(&accepted, &seen), VersionCheck::Same);
    }
}
