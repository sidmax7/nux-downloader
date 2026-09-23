//! Pure parts of probing (`ARCHITECTURE.md` §5): parsing the headers a
//! probe response carries, and sanitizing the filename that comes out of
//! them. The I/O — sending the ranged `GET`, following redirects — is
//! built in Phase 2; this module has none.

use url::Url;

/// A parsed `Content-Range` header (`bytes start-end/total`), used both
/// to read a probe's total size and, later, to validate a chunk
/// response actually served what was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentRange {
    pub start: u64,
    pub end: u64,
    /// `None` when the server sent `*` for the total (doesn't know it).
    pub total: Option<u64>,
}

/// Parses `bytes <start>-<end>/<total-or-*>`, case-insensitive on
/// `bytes`, tolerant of surrounding whitespace. `None` for anything that
/// doesn't fit that shape, including `end < start`.
#[must_use]
pub fn parse_content_range(value: &str) -> Option<ContentRange> {
    let s = value.trim();
    if s.len() < 5 || !s[..5].eq_ignore_ascii_case("bytes") {
        return None;
    }
    let rest = s[5..].trim_start();
    let (start_str, rest) = rest.split_once('-')?;
    let (end_str, total_str) = rest.split_once('/')?;

    let start: u64 = start_str.trim().parse().ok()?;
    let end: u64 = end_str.trim().parse().ok()?;
    if end < start {
        return None;
    }

    let total_str = total_str.trim();
    let total = if total_str == "*" {
        None
    } else {
        Some(total_str.parse().ok()?)
    };

    Some(ContentRange { start, end, total })
}

/// Percent-decodes `value`, falling back to the raw string if it isn't
/// valid UTF-8 once decoded — the same fallback `decodeURIComponent`
/// gets via a `try`/`catch` in the original Plexo parser.
fn percent_decode_or_raw(value: &str) -> String {
    percent_encoding::percent_decode_str(value)
        .decode_utf8()
        .map(std::borrow::Cow::into_owned)
        .unwrap_or_else(|_| value.to_string())
}

/// A crude `\b` word boundary: finds `needle` in `haystack`
/// case-insensitively, requiring the match start at the beginning of the
/// string or after a non-alphanumeric, non-`_` byte. Stops a `filename=`
/// search from matching inside some other parameter's name.
fn find_at_word_boundary(haystack: &str, needle: &str) -> Option<usize> {
    let haystack_lower = haystack.to_ascii_lowercase();
    let needle_lower = needle.to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(relative) = haystack_lower.get(search_from..)?.find(&needle_lower) {
        let index = search_from + relative;
        let boundary_ok = index == 0 || {
            let before = haystack.as_bytes()[index - 1];
            !before.is_ascii_alphanumeric() && before != b'_'
        };
        if boundary_ok {
            return Some(index);
        }
        search_from = index + 1;
    }
    None
}

/// RFC 6266 / RFC 5987 `filename*=<charset>'<lang>'<percent-encoded>`.
/// Takes priority over the plain `filename=` forms below when present.
fn extract_extended_filename(disposition: &str) -> Option<String> {
    let start = find_at_word_boundary(disposition, "filename*=")?;
    let after = &disposition[start + "filename*=".len()..];

    let charset_end = after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))?;
    if charset_end == 0 {
        return None;
    }
    let after_charset = after[charset_end..].strip_prefix('\'')?;
    let lang_end = after_charset.find('\'')?;
    let after_lang = &after_charset[lang_end + 1..];

    let value_end = after_lang
        .find(|c: char| c == ';' || c.is_whitespace())
        .unwrap_or(after_lang.len());
    let value = &after_lang[..value_end];
    if value.is_empty() {
        None
    } else {
        Some(percent_decode_or_raw(value))
    }
}

/// `filename="quoted, possibly with \" escapes"`. Preserves a literal
/// `;` inside the quotes, unlike the unquoted token form.
fn extract_quoted_filename(disposition: &str) -> Option<String> {
    let start = find_at_word_boundary(disposition, "filename=")?;
    let mut chars = disposition[start + "filename=".len()..].chars();
    if chars.next() != Some('"') {
        return None;
    }

    let mut unescaped = String::new();
    loop {
        match chars.next()? {
            '\\' => unescaped.push(chars.next()?),
            '"' => return Some(percent_decode_or_raw(&unescaped)),
            other => unescaped.push(other),
        }
    }
}

/// `filename=bare-token` — no quotes, ends at the next `;` or whitespace.
fn extract_token_filename(disposition: &str) -> Option<String> {
    let start = find_at_word_boundary(disposition, "filename=")?;
    let after = &disposition[start + "filename=".len()..];
    let end = after
        .find(|c: char| c == ';' || c.is_whitespace())
        .unwrap_or(after.len());
    let value = &after[..end];
    if value.is_empty() {
        None
    } else {
        Some(percent_decode_or_raw(value))
    }
}

/// The filename a `Content-Disposition` header names, if any — extended
/// (`filename*=`) first, then quoted, then a bare token.
#[must_use]
pub fn parse_content_disposition_filename(disposition: &str) -> Option<String> {
    extract_extended_filename(disposition)
        .or_else(|| extract_quoted_filename(disposition))
        .or_else(|| extract_token_filename(disposition))
}

/// The last non-empty, percent-decoded path segment of `url`, or
/// `"download"` if it has none (e.g. `https://example.com/`).
#[must_use]
pub fn filename_from_url(url: &Url) -> String {
    let decoded = percent_decode_or_raw(url.path());
    decoded
        .split('/')
        .rfind(|segment| !segment.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "download".to_string())
}

/// The filename a probe response suggests: `Content-Disposition` if it
/// names one, otherwise the URL's own path.
#[must_use]
pub fn suggested_filename(disposition: Option<&str>, url: &Url) -> String {
    disposition
        .and_then(parse_content_disposition_filename)
        .unwrap_or_else(|| filename_from_url(url))
}

/// Filesystem name-length limits vary (255 bytes is the common ext4 /
/// most-filesystems `NAME_MAX`, and comfortably under Windows' 260-char
/// `MAX_PATH` minus a directory prefix); this keeps every sanitized name
/// safely under all of them rather than tracking each platform's exact
/// limit.
const MAX_FILENAME_BYTES: usize = 255;

/// Whether `name`'s part before its first `.` (or the whole name, if it
/// has none) is a Windows-reserved device name: `CON`, `PRN`, `AUX`,
/// `NUL`, or `COM`/`LPT` followed by a single digit 1–9 (ASCII or the
/// superscript ¹²³ some Windows APIs also treat as reserved).
fn is_windows_reserved(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or(name).to_ascii_lowercase();
    if matches!(base.as_str(), "con" | "prn" | "aux" | "nul") {
        return true;
    }
    let Some(suffix) = base
        .strip_prefix("com")
        .or_else(|| base.strip_prefix("lpt"))
    else {
        return false;
    };
    matches!(
        suffix,
        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
    )
}

/// Truncates to at most `max_bytes` bytes, backing off to the nearest
/// earlier UTF-8 character boundary rather than splitting one.
fn truncate_to_byte_limit(mut s: String, max_bytes: usize) -> String {
    if s.len() > max_bytes {
        let mut cut = max_bytes;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

/// Makes a server-supplied name safe to write as one path component on
/// any of Linux, macOS or Windows (`ARCHITECTURE.md` §9.2) — regardless
/// of which platform this binary is running on, so the same input
/// sanitizes identically in every CI run and a name written on one OS
/// stays safe if the download folder is later opened on another.
#[must_use]
pub fn sanitize_filename(name: &str) -> String {
    let mut safe: String = name
        .chars()
        .map(|c| {
            if c == '\\' || c == '/' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();

    safe = safe
        .chars()
        .map(|c| if "<>:\"|?*".contains(c) { '_' } else { c })
        .collect();

    let trimmed_len = safe.trim_end_matches([' ', '.']).len();
    safe.truncate(trimmed_len);

    if is_windows_reserved(&safe) {
        safe.insert(0, '_');
    }

    if safe.is_empty() || safe.chars().all(|c| c == '.') {
        return "download".to_string();
    }

    truncate_to_byte_limit(safe, MAX_FILENAME_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_content_range ---

    #[test]
    fn parses_a_bounded_range_with_known_total() {
        assert_eq!(
            parse_content_range("bytes 0-99/100"),
            Some(ContentRange {
                start: 0,
                end: 99,
                total: Some(100)
            })
        );
    }

    #[test]
    fn parses_an_unknown_total_as_a_star() {
        assert_eq!(
            parse_content_range("bytes 0-0/*"),
            Some(ContentRange {
                start: 0,
                end: 0,
                total: None
            })
        );
    }

    #[test]
    fn is_case_insensitive_and_tolerates_whitespace() {
        assert_eq!(
            parse_content_range("  BYTES  0-0 / * "),
            Some(ContentRange {
                start: 0,
                end: 0,
                total: None
            })
        );
    }

    #[test]
    fn rejects_end_before_start() {
        assert_eq!(parse_content_range("bytes 99-0/100"), None);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_content_range("not a content range"), None);
        assert_eq!(parse_content_range(""), None);
    }

    // --- Content-Disposition filename ---

    #[test]
    fn parses_a_quoted_filename() {
        assert_eq!(
            parse_content_disposition_filename("attachment; filename=\"report.pdf\""),
            Some("report.pdf".to_string())
        );
    }

    #[test]
    fn a_quoted_filename_preserves_an_internal_semicolon() {
        assert_eq!(
            parse_content_disposition_filename("attachment; filename=\"report; final.pdf\""),
            Some("report; final.pdf".to_string())
        );
    }

    #[test]
    fn a_quoted_filename_unescapes_backslashes() {
        assert_eq!(
            parse_content_disposition_filename("attachment; filename=\"esc\\\"aped\\\\.pdf\""),
            Some("esc\"aped\\.pdf".to_string())
        );
    }

    #[test]
    fn parses_an_unquoted_token() {
        assert_eq!(
            parse_content_disposition_filename("attachment; filename=report.pdf"),
            Some("report.pdf".to_string())
        );
    }

    #[test]
    fn parses_an_rfc5987_extended_value() {
        assert_eq!(
            parse_content_disposition_filename("attachment; filename*=UTF-8''caf%C3%A9.pdf"),
            Some("café.pdf".to_string())
        );
    }

    #[test]
    fn extended_value_takes_priority_over_a_quoted_fallback() {
        let header = "attachment; filename=\"fallback.pdf\"; filename*=UTF-8''real.pdf";
        assert_eq!(
            parse_content_disposition_filename(header),
            Some("real.pdf".to_string())
        );
    }

    #[test]
    fn no_filename_parameter_is_none() {
        assert_eq!(parse_content_disposition_filename("attachment"), None);
        assert_eq!(parse_content_disposition_filename(""), None);
    }

    // --- URL fallback and combination ---

    #[test]
    fn filename_from_url_takes_the_last_path_segment() {
        let url = Url::parse("https://example.com/dir/report.pdf").unwrap();
        assert_eq!(filename_from_url(&url), "report.pdf");
    }

    #[test]
    fn filename_from_url_decodes_percent_escapes() {
        let url = Url::parse("https://example.com/na%20me.pdf").unwrap();
        assert_eq!(filename_from_url(&url), "na me.pdf");
    }

    #[test]
    fn filename_from_url_falls_back_to_download_with_no_path() {
        let url = Url::parse("https://example.com/").unwrap();
        assert_eq!(filename_from_url(&url), "download");
    }

    #[test]
    fn suggested_filename_prefers_disposition_over_the_url() {
        let url = Url::parse("https://example.com/ignored.bin").unwrap();
        assert_eq!(
            suggested_filename(Some("attachment; filename=\"real.pdf\""), &url),
            "real.pdf"
        );
    }

    #[test]
    fn suggested_filename_falls_back_to_the_url_with_no_disposition() {
        let url = Url::parse("https://example.com/file.bin").unwrap();
        assert_eq!(suggested_filename(None, &url), "file.bin");
    }

    // --- sanitize_filename ---

    #[test]
    fn strips_path_separators_defeating_traversal() {
        let sanitized = sanitize_filename("../../etc/passwd");
        assert!(!sanitized.contains('/'));
        assert!(!sanitized.contains('\\'));
        assert_eq!(sanitized, ".._.._etc_passwd");
    }

    #[test]
    fn escapes_a_reserved_windows_device_name() {
        assert_eq!(sanitize_filename("CON.txt"), "_CON.txt");
        assert_eq!(sanitize_filename("LPT1"), "_LPT1");
        assert_eq!(sanitize_filename("nul"), "_nul");
    }

    #[test]
    fn does_not_flag_a_similar_but_non_reserved_name() {
        assert_eq!(sanitize_filename("LPT10"), "LPT10");
        assert_eq!(sanitize_filename("console.txt"), "console.txt");
    }

    #[test]
    fn replaces_control_characters_including_nul() {
        let sanitized = sanitize_filename("file\u{0}name.txt");
        assert_eq!(sanitized, "file_name.txt");
    }

    #[test]
    fn truncates_a_very_long_name_at_a_char_boundary() {
        let long_name = "a".repeat(500);
        let sanitized = sanitize_filename(&long_name);
        assert!(sanitized.len() <= MAX_FILENAME_BYTES);
        assert!(sanitized.is_char_boundary(sanitized.len()));
    }

    #[test]
    fn truncates_a_long_multibyte_name_without_splitting_a_character() {
        let long_name = "é".repeat(300); // each 'é' is 2 bytes in UTF-8
        let sanitized = sanitize_filename(&long_name);
        assert!(sanitized.len() <= MAX_FILENAME_BYTES);
        assert!(String::from_utf8(sanitized.into_bytes()).is_ok());
    }

    #[test]
    fn empty_or_all_dots_becomes_download() {
        assert_eq!(sanitize_filename(""), "download");
        assert_eq!(sanitize_filename("..."), "download");
    }

    #[test]
    fn trims_trailing_dots_and_spaces() {
        assert_eq!(sanitize_filename("trailing.dots... "), "trailing.dots");
    }

    #[test]
    fn replaces_windows_illegal_characters_on_every_platform() {
        assert_eq!(sanitize_filename("a<b>c:d\"e|f?g*h"), "a_b_c_d_e_f_g_h");
    }
}
