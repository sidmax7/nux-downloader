//! Server-supplied header values and the file names made from them.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuxd_core::probe::{
    parse_content_disposition_filename, parse_content_range, sanitize_filename, suggested_filename,
};

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let _ = parse_content_range(&text);
    let _ = parse_content_disposition_filename(&text);

    let clean = sanitize_filename(&text);
    assert!(!clean.is_empty(), "empty name from {text:?}");
    assert!(clean.len() <= 255, "name too long: {}", clean.len());
    for bad in ['/', '\\', '\0'] {
        assert!(!clean.contains(bad), "{bad:?} survived in {clean:?}");
    }
    assert!(clean != "." && clean != "..", "dot name from {text:?}");
    // Cleaning is idempotent: a clean name stays as it is.
    assert_eq!(sanitize_filename(&clean), clean, "not idempotent for {text:?}");

    let url = url::Url::parse("https://example.com/dir/file.bin").unwrap();
    // The probe sanitizes whatever the server suggests.
    let name = sanitize_filename(&suggested_filename(Some(&text), &url));
    assert!(!name.is_empty());
    assert!(!name.contains('/') && !name.contains('\\') && !name.contains('\0'));
});
