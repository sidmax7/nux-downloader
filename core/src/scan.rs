//! Page scanning (`ARCHITECTURE.md` §13.1, Decision 35): when a pasted
//! link turns out to be a web page, find the files it links to or embeds.
//!
//! The extractor is deliberately small and tolerant: it only needs
//! attribute values and bare addresses, never the document tree. It runs
//! no JavaScript, keeps no cookies and solves no challenges; a page that
//! refuses an automated visit is reported, not worked around (scope
//! guardrails in `CLAUDE.md`).

use reqwest::header::{ACCEPT, CONTENT_TYPE};
use std::collections::HashSet;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

/// A page body beyond this is cut off (its links are rarely past it).
pub const MAX_PAGE_BYTES: usize = 4 * 1024 * 1024;
/// How long one page may take, headers and body together.
pub const PAGE_TIMEOUT: Duration = Duration::from_secs(20);
/// Links kept from one page.
pub const MAX_LINKS: usize = 5_000;
/// Pages opened at once on one host (a scan opens many, so more than the
/// probe pool's per-host cap; still polite).
pub const PAGES_PER_HOST: usize = 6;
/// Why a page couldn't be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// The address answered with something that isn't a web page (a file).
    NotAPage,
    /// The page couldn't be read, with a reason fit to show.
    Failed(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAPage => f.write_str("not a web page"),
            Self::Failed(reason) => f.write_str(reason),
        }
    }
}
/// Same-site pages a deep scan follows, in total.
pub const MAX_DEEP_PAGES: usize = 100;

/// What a page offers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Page {
    /// The `<title>`, cleaned up; names the files' package.
    pub title: Option<String>,
    /// Every `http(s)` link found, absolute, without fragments, in page
    /// order, each once.
    pub links: Vec<Url>,
}

/// Attributes that hold one address.
const URL_ATTRIBUTES: &[&str] = &[
    "href",
    "src",
    "data-src",
    "data-href",
    "data-url",
    "data-original",
    "data-file",
    "data-download",
    "poster",
    "content",
];
/// Attributes that hold a `srcset` list (`url 2x, url 640w`).
const SRCSET_ATTRIBUTES: &[&str] = &["srcset", "data-srcset"];

/// Finds the title and links in `html`, resolving relative addresses
/// against `page_url` (or the page's `<base href>`).
#[must_use]
pub fn extract(html: &str, page_url: &Url) -> Page {
    let mut base = page_url.clone();
    let mut title = None;
    let mut raw: Vec<String> = Vec::new();

    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if html[i..].starts_with("<!--") {
            i = html[i + 4..]
                .find("-->")
                .map_or(bytes.len(), |end| i + 4 + end + 3);
            continue;
        }
        let Some((tag, attributes, end)) = parse_tag(html, i) else {
            i += 1;
            continue;
        };
        i = end;
        match tag.as_str() {
            "base" => {
                if let Some(href) = attributes.iter().find(|(n, _)| n == "href")
                    && let Ok(url) = page_url.join(&decode_entities(&href.1))
                {
                    base = url;
                }
            }
            "title" if title.is_none() => {
                if let Some(close) = find_ignore_case(&html[i..], "</title") {
                    title = clean_title(&html[i..i + close]);
                }
            }
            _ => {}
        }
        for (name, value) in &attributes {
            if URL_ATTRIBUTES.contains(&name.as_str()) {
                // `content` holds plenty that isn't an address; only an
                // absolute one counts (`og:video`, refresh targets…).
                let value = decode_entities(value);
                if name == "content" {
                    if let Some(at) = value.find("http") {
                        raw.push(value[at..].to_string());
                    }
                } else {
                    raw.push(value);
                }
            } else if SRCSET_ATTRIBUTES.contains(&name.as_str()) {
                for candidate in decode_entities(value).split(',') {
                    if let Some(url) = candidate.split_whitespace().next() {
                        raw.push(url.to_string());
                    }
                }
            }
        }
    }

    // Bare addresses anywhere: in text, in scripts (players' `file:`
    // settings), JSON with escaped slashes.
    let unescaped = html.replace("\\/", "/");
    raw.extend(bare_urls(&unescaped));

    let mut seen = HashSet::new();
    let mut links = Vec::new();
    for value in raw {
        let value = value.trim();
        if value.is_empty() || value.starts_with('#') {
            continue;
        }
        let Ok(mut url) = base.join(value) else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            continue;
        }
        url.set_fragment(None);
        if seen.insert(url.to_string()) {
            links.push(url);
            if links.len() >= MAX_LINKS {
                break;
            }
        }
    }
    Page { title, links }
}

/// Parses the tag starting at `<` at `start`: its lowercase name, its
/// attributes (lowercase names, raw values) and the index just past `>`.
/// `None` for anything that isn't an opening tag.
/// An opening tag: its lowercase name, its attributes (lowercase names,
/// raw values) and the index just past its `>`.
type Tag = (String, Vec<(String, String)>, usize);

fn parse_tag(html: &str, start: usize) -> Option<Tag> {
    let bytes = html.as_bytes();
    let mut i = start + 1;
    if !bytes.get(i)?.is_ascii_alphabetic() {
        return None;
    }
    let name_start = i;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-') {
        i += 1;
    }
    let tag = html[name_start..i].to_ascii_lowercase();
    let mut attributes = Vec::new();
    loop {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        match bytes.get(i) {
            None => return Some((tag, attributes, bytes.len())),
            Some(b'>') => return Some((tag, attributes, i + 1)),
            _ => {}
        }
        let attr_start = i;
        while i < bytes.len()
            && !matches!(bytes[i], b'=' | b'>' | b'/')
            && !bytes[i].is_ascii_whitespace()
        {
            i += 1;
        }
        let name = html[attr_start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if bytes.get(i) != Some(&b'=') {
            if name.is_empty() {
                i += 1; // a stray character; skip it
            }
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let value = match bytes.get(i) {
            Some(&quote @ (b'"' | b'\'')) => {
                let value_start = i + 1;
                let close = html[value_start..]
                    .find(quote as char)
                    .map_or(bytes.len(), |n| value_start + n);
                i = (close + 1).min(bytes.len());
                &html[value_start..close]
            }
            _ => {
                let value_start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
                    i += 1;
                }
                &html[value_start..i]
            }
        };
        if !name.is_empty() {
            attributes.push((name, value.to_string()));
        }
    }
}

/// `http(s)://…` runs in free text, without trailing punctuation.
fn bare_urls(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("http") {
        let candidate = &rest[at..];
        let scheme_len = if candidate.starts_with("https://") {
            8
        } else if candidate.starts_with("http://") {
            7
        } else {
            rest = &rest[at + 4..];
            continue;
        };
        let end = candidate
            .char_indices()
            .find(|&(_, c)| {
                c.is_whitespace()
                    || matches!(
                        c,
                        '"' | '\'' | '<' | '>' | '`' | '\\' | '{' | '}' | '|' | '^'
                    )
            })
            .map_or(candidate.len(), |(n, _)| n);
        let url = candidate[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']']);
        if url.len() > scheme_len {
            found.push(decode_entities(url));
        }
        rest = &candidate[end.max(scheme_len)..];
    }
    found
}

/// The HTML character references that show up in addresses and titles.
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let decoded = tail.find(';').filter(|&end| end <= 10).and_then(|end| {
            let entity = &tail[1..end];
            let ch = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ => entity.strip_prefix('#').and_then(|number| {
                    let code = match number.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok(),
                        None => number.parse().ok(),
                    };
                    code.and_then(char::from_u32)
                }),
            }?;
            Some((ch, end + 1))
        });
        match decoded {
            Some((ch, len)) => {
                out.push(ch);
                rest = &tail[len..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn find_ignore_case(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn clean_title(raw: &str) -> Option<String> {
    let text = decode_entities(raw);
    let title = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!title.is_empty()).then(|| title.chars().take(150).collect())
}

/// File extensions that mark a link as a file rather than a page.
const FILE_EXTENSIONS: &[&str] = &[
    // archives
    "zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "tbz2", "xz", "txz", "zst", "lz", "lzma", "cab",
    // disk images and installers
    "iso", "img", "dmg", "vhd", "vhdx", "qcow2", "exe", "msi", "deb", "rpm", "apk", "appimage",
    "pkg", "flatpak", "jar", // video
    "mp4", "mkv", "webm", "avi", "mov", "wmv", "flv", "m4v", "ts", "mpg", "mpeg", "3gp",
    // audio
    "mp3", "flac", "wav", "ogg", "opus", "m4a", "aac", "wma", // images
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "heic", "avif",
    // documents and comics
    "pdf", "epub", "mobi", "azw3", "cbz", "cbr", "djvu", "doc", "docx", "xls", "xlsx", "ppt",
    "pptx", "odt", "ods", "odp", // checksums and other
    "sfv", "md5", "sha1", "sha256", "sha512", "torrent", "bin",
];

/// Whether `url` points at a file, judged by its name: a known extension,
/// or a split-archive volume (`.r00`, `.001`).
#[must_use]
pub fn is_file_link(url: &Url) -> bool {
    let Some(name) = url.path_segments().and_then(|mut s| s.next_back()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    let Some((_, extension)) = name.rsplit_once('.') else {
        return false;
    };
    if FILE_EXTENSIONS.contains(&extension) {
        return true;
    }
    let digits = extension.trim_start_matches('r');
    extension.len() == 3 && digits.len() >= 2 && digits.bytes().all(|b| b.is_ascii_digit())
}

/// Whether `link` is on the same site as `page`: the same host, or one is
/// a subdomain of the other (`www.`, `cdn.`).
#[must_use]
pub fn same_site(page: &Url, link: &Url) -> bool {
    let (Some(a), Some(b)) = (page.host_str(), link.host_str()) else {
        return false;
    };
    let (a, b) = (a.to_ascii_lowercase(), b.to_ascii_lowercase());
    a == b || a.ends_with(&format!(".{b}")) || b.ends_with(&format!(".{a}"))
}

/// Fetches `url` and extracts its links, returning the address the
/// answer came from (after redirects) with them. A refusal
/// (`403`/`429`/`503`) says the site may want a browser check, which this
/// app does not attempt. An answer that is not HTML is `NotAPage`, and its
/// body is never read.
pub async fn fetch_page(
    client: &reqwest::Client,
    url: &Url,
    cancel: &CancellationToken,
) -> Result<(Url, Page), FetchError> {
    let work = async {
        let mut response = client
            .get(url.clone())
            .header(ACCEPT, "text/html,application/xhtml+xml;q=0.9,*/*;q=0.5")
            .send()
            .await
            .map_err(|error| FetchError::Failed(format!("couldn't read the page: {error}")))?;
        let status = response.status().as_u16();
        if matches!(status, 403 | 429 | 503) {
            return Err(FetchError::Failed(format!(
                "the site refused to show the page ({status}); it may want a browser check, which this app doesn't attempt"
            )));
        }
        if !response.status().is_success() {
            return Err(FetchError::Failed(format!("the page answered {status}")));
        }
        let html = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("html"));
        if !html {
            return Err(FetchError::NotAPage);
        }
        let page_url = response.url().clone();
        let mut body = Vec::new();
        while body.len() < MAX_PAGE_BYTES {
            match response.chunk().await {
                Ok(Some(chunk)) => body.extend_from_slice(&chunk),
                Ok(None) => break,
                Err(error) => {
                    return Err(FetchError::Failed(format!(
                        "couldn't read the page: {error}"
                    )));
                }
            }
        }
        body.truncate(MAX_PAGE_BYTES);
        let page = extract(&String::from_utf8_lossy(&body), &page_url);
        Ok((page_url, page))
    };
    tokio::select! {
        () = cancel.cancelled() => Err(FetchError::Failed("cancelled".to_string())),
        result = tokio::time::timeout(PAGE_TIMEOUT, work) => {
            result.unwrap_or_else(|_| {
                Err(FetchError::Failed("the page took too long to load".to_string()))
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> Url {
        Url::parse("https://example.com/album/a1?x=1").unwrap_or_else(|e| panic!("{e}"))
    }

    fn links(html: &str) -> Vec<String> {
        extract(html, &page())
            .links
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn finds_links_and_embedded_media_and_resolves_them() {
        let html = r#"
            <a href="/files/one.zip">one</a>
            <A HREF='two.mp4'>two</A>
            <img src="//cdn.example.com/p/3.jpg" alt=x>
            <video poster=/v/poster.webp><source src="https://media.example.net/v.webm" type="video/webm"></video>
            <img srcset="/s/small.png 1x, /s/big.png 2x">
            <div data-src="/lazy/4.png"></div>
        "#;
        assert_eq!(
            links(html),
            [
                "https://example.com/files/one.zip",
                "https://example.com/album/two.mp4",
                "https://cdn.example.com/p/3.jpg",
                "https://example.com/v/poster.webp",
                "https://media.example.net/v.webm",
                "https://example.com/s/small.png",
                "https://example.com/s/big.png",
                "https://example.com/lazy/4.png",
            ]
        );
    }

    #[test]
    fn decodes_entities_drops_fragments_and_duplicates() {
        let html = r#"<a href="/get?id=1&amp;name=a%20b.zip#top">x</a>
                      <a href="/get?id=1&#38;name=a%20b.zip">again</a>"#;
        assert_eq!(links(html), ["https://example.com/get?id=1&name=a%20b.zip"]);
    }

    #[test]
    fn skips_what_is_not_a_web_address() {
        let html = r##"<a href="#top">t</a><a href="mailto:a@b.c">m</a>
                       <a href="javascript:void(0)">j</a><img src="data:image/png;base64,AAAA">
                       <a href="ftp://x/y.zip">f</a>"##;
        assert!(links(html).is_empty(), "{:?}", links(html));
    }

    #[test]
    fn honours_base_href() {
        let html = r#"<base href="https://files.example.org/root/"><a href="a.rar">a</a>"#;
        let found = links(html);
        assert!(
            found.contains(&"https://files.example.org/root/a.rar".to_string()),
            "{found:?}"
        );
        assert!(
            !found.iter().any(|l| l.starts_with("https://example.com")),
            "{found:?}"
        );
    }

    #[test]
    fn finds_bare_addresses_in_text_and_scripts() {
        let html = r#"<p>Mirror: https://dl.example.com/x.7z.</p>
                      <script>player({"file":"https:\/\/v.example.com\/clip.mp4"})</script>"#;
        assert_eq!(
            links(html),
            [
                "https://dl.example.com/x.7z",
                "https://v.example.com/clip.mp4"
            ]
        );
    }

    #[test]
    fn ignores_comments_and_takes_the_title() {
        let html = "<title>\n  My &amp; Album\n</title><!-- <a href=\"/hidden.zip\"> -->";
        let found = extract(html, &page());
        assert_eq!(found.title.as_deref(), Some("My & Album"));
        assert!(found.links.is_empty(), "{:?}", found.links);
    }

    #[test]
    fn survives_broken_markup() {
        for html in [
            "<",
            "<a",
            "<a href=",
            "<a href=\"unterminated",
            "<<<>>>",
            "&#xFFFFFFFF;",
            "<a =x>",
        ] {
            let _ = extract(html, &page());
        }
    }

    #[test]
    fn knows_a_file_link_by_its_name() {
        for yes in [
            "https://x/a.zip",
            "https://x/a.part01.rar",
            "https://x/A.MKV",
            "https://x/a.7z.001",
            "https://x/a.r00",
            "https://x/a.jpg?w=1",
        ] {
            assert!(
                is_file_link(&Url::parse(yes).unwrap_or_else(|e| panic!("{e}"))),
                "{yes}"
            );
        }
        for no in [
            "https://x/",
            "https://x/album/abc",
            "https://x/page.html",
            "https://x/a.php?f=1.zip",
        ] {
            assert!(
                !is_file_link(&Url::parse(no).unwrap_or_else(|e| panic!("{e}"))),
                "{no}"
            );
        }
    }

    #[test]
    fn same_site_includes_subdomains_only() {
        let page = page();
        let at = |s: &str| Url::parse(s).unwrap_or_else(|e| panic!("{e}"));
        assert!(same_site(&page, &at("https://example.com/f/1")));
        assert!(same_site(&page, &at("https://cdn.example.com/f/1")));
        assert!(!same_site(&page, &at("https://notexample.com/f/1")));
        assert!(!same_site(&page, &at("https://other.org/f/1")));
    }
}
