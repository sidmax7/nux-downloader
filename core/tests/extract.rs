//! Extraction (`IMPLEMENTATION_PLAN.md` P5.3, P5.4, P5.6): zip and 7z
//! fixtures are generated here, byte-exact round trips are checked, and a
//! set of hostile archives must be refused without touching anything
//! outside the output directory.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nuxd_core::error::ExtractError;
use nuxd_core::extract::{ExtractLimits, ExtractSpec, extract};
use nuxd_core::model::ArchiveScheme;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

fn limits() -> ExtractLimits {
    ExtractLimits {
        max_bytes: 1 << 30,
        max_ratio: 1000,
        ratio_floor: 1 << 30,
        max_entries: 1000,
    }
}

fn pseudo(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    (0..len)
        .map(|_| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 33) as u8
        })
        .collect()
}

fn files() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("top.txt", b"hello".to_vec()),
        ("dir/nested/data.bin", pseudo(300_000, 1)),
        ("dir/empty.txt", Vec::new()),
        ("other.bin", pseudo(70_000, 2)),
    ]
}

fn make_zip(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, data) in entries {
        writer.start_file(*name, options).unwrap();
        writer.write_all(data).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn make_7z(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut writer = sevenz_rust2::ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
    for (name, data) in entries {
        writer
            .push_archive_entry(
                sevenz_rust2::ArchiveEntry::new_file(name),
                Some(Cursor::new(data.clone())),
            )
            .unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn write_volumes(dir: &Path, name: &str, bytes: &[u8], parts: usize) -> Vec<PathBuf> {
    let chunk = bytes.len().div_ceil(parts);
    bytes
        .chunks(chunk)
        .enumerate()
        .map(|(i, part)| {
            let path = dir.join(format!("{name}.{:03}", i + 1));
            std::fs::write(&path, part).unwrap();
            path
        })
        .collect()
}

struct Run {
    out: tempfile::TempDir,
}

fn spec(scheme: ArchiveScheme, base: &str, volumes: Vec<PathBuf>, dest: &Path) -> ExtractSpec {
    ExtractSpec {
        scheme,
        base_name: base.to_string(),
        volumes,
        dest_dir: dest.to_path_buf(),
        limits: limits(),
    }
}

fn run(
    scheme: ArchiveScheme,
    base: &str,
    volumes: Vec<PathBuf>,
    limits_override: Option<ExtractLimits>,
) -> (
    Run,
    Result<nuxd_core::extract::ExtractSummary, ExtractError>,
) {
    let out = tempfile::tempdir().unwrap();
    let mut spec = spec(scheme, base, volumes, out.path());
    if let Some(l) = limits_override {
        spec.limits = l;
    }
    let cancel = AtomicBool::new(false);
    let result = extract(&spec, &cancel, &mut |_| {});
    (Run { out }, result)
}

fn assert_files(root: &Path) {
    for (name, data) in files() {
        let got = std::fs::read(root.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(got, data, "{name} differs");
    }
}

/// No staging leftovers beside the output.
fn assert_clean(root: &Path) {
    let names: Vec<_> = std::fs::read_dir(root)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().all(|n| !n.contains("nuxdextract")),
        "staging left behind: {names:?}"
    );
}

#[test]
fn zip_round_trips_byte_exact() {
    let src = tempfile::tempdir().unwrap();
    let archive = src.path().join("a.zip");
    std::fs::write(&archive, make_zip(&files())).unwrap();
    let (run, result) = run(ArchiveScheme::SplitZip, "a", vec![archive], None);
    let summary = result.unwrap();
    assert_eq!(summary.entries, 4);
    assert_files(run.out.path());
    assert_clean(run.out.path());
}

#[test]
fn seven_zip_round_trips_byte_exact() {
    let src = tempfile::tempdir().unwrap();
    let archive = src.path().join("a.7z");
    std::fs::write(&archive, make_7z(&files())).unwrap();
    let (run, result) = run(ArchiveScheme::SevenZip, "a.7z", vec![archive], None);
    result.unwrap();
    assert_files(run.out.path());
    assert_clean(run.out.path());
}

#[test]
fn seven_zip_volumes_extract_across_boundaries() {
    let src = tempfile::tempdir().unwrap();
    let volumes = write_volumes(src.path(), "a.7z", &make_7z(&files()), 4);
    assert_eq!(volumes.len(), 4);
    let (run, result) = run(ArchiveScheme::SevenZip, "a.7z", volumes, None);
    result.unwrap();
    assert_files(run.out.path());
}

#[test]
fn a_zip_cut_into_generic_parts_is_joined_and_extracted() {
    let src = tempfile::tempdir().unwrap();
    let volumes = write_volumes(src.path(), "a.zip", &make_zip(&files()), 3);
    let (run, result) = run(ArchiveScheme::GenericSplit, "a.zip", volumes, None);
    result.unwrap();
    assert_files(run.out.path());
}

#[test]
fn a_generic_split_of_something_else_is_just_joined() {
    let src = tempfile::tempdir().unwrap();
    let data = pseudo(100_000, 9);
    let volumes = write_volumes(src.path(), "movie.mkv", &data, 3);
    let (run, result) = run(ArchiveScheme::GenericSplit, "movie.mkv", volumes, None);
    result.unwrap();
    assert_eq!(
        std::fs::read(run.out.path().join("movie.mkv")).unwrap(),
        data
    );
}

#[test]
fn nothing_is_overwritten() {
    let src = tempfile::tempdir().unwrap();
    let archive = src.path().join("a.zip");
    std::fs::write(&archive, make_zip(&[("top.txt", b"new".to_vec())])).unwrap();
    let out = tempfile::tempdir().unwrap();
    std::fs::write(out.path().join("top.txt"), b"precious").unwrap();
    let cancel = AtomicBool::new(false);
    extract(
        &spec(ArchiveScheme::SplitZip, "a", vec![archive], out.path()),
        &cancel,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(
        std::fs::read(out.path().join("top.txt")).unwrap(),
        b"precious"
    );
    assert_eq!(
        std::fs::read(out.path().join("top (1).txt")).unwrap(),
        b"new"
    );
}

#[test]
fn progress_is_reported() {
    let src = tempfile::tempdir().unwrap();
    let archive = src.path().join("a.zip");
    std::fs::write(&archive, make_zip(&files())).unwrap();
    let out = tempfile::tempdir().unwrap();
    let cancel = AtomicBool::new(false);
    let mut last = None;
    extract(
        &spec(ArchiveScheme::SplitZip, "a", vec![archive], out.path()),
        &cancel,
        &mut |p| last = Some(p),
    )
    .unwrap();
    let last = last.unwrap();
    assert_eq!(last.bytes_done, 5 + 300_000 + 70_000);
    assert_eq!(last.bytes_total, last.bytes_done);
}

// --- failure and safety (P5.4) ---

#[test]
fn a_corrupt_zip_is_reported_as_corrupt() {
    let src = tempfile::tempdir().unwrap();
    let mut bytes = make_zip(&[("a.bin", pseudo(50_000, 3))]);
    // Flip a byte in the compressed data.
    bytes[2000] ^= 0xFF;
    let archive = src.path().join("a.zip");
    std::fs::write(&archive, bytes).unwrap();
    let (run, result) = run(ArchiveScheme::SplitZip, "a", vec![archive], None);
    assert!(
        matches!(result, Err(ExtractError::Corrupt { .. })),
        "{result:?}"
    );
    assert!(!run.out.path().join("a.bin").exists());
    assert_clean(run.out.path());
}

#[test]
fn a_truncated_seven_zip_fails() {
    let src = tempfile::tempdir().unwrap();
    let mut bytes = make_7z(&files());
    bytes.truncate(bytes.len() / 2);
    let archive = src.path().join("a.7z");
    std::fs::write(&archive, bytes).unwrap();
    let (run, result) = run(ArchiveScheme::SevenZip, "a.7z", vec![archive], None);
    assert!(result.is_err());
    assert_clean(run.out.path());
}

fn assert_unsafe(result: &Result<nuxd_core::extract::ExtractSummary, ExtractError>) {
    assert!(
        matches!(result, Err(ExtractError::UnsafePath { .. })),
        "expected UnsafePath, got {result:?}"
    );
}

#[test]
fn path_traversal_is_refused_in_zip_and_7z() {
    for bad in [
        "../evil.txt",
        "dir/../../evil.txt",
        "/abs/evil.txt",
        "..\\evil.txt",
        "C:\\evil.txt",
    ] {
        let src = tempfile::tempdir().unwrap();
        let archive = src.path().join("a.zip");
        std::fs::write(
            &archive,
            make_zip(&[("fine.txt", b"ok".to_vec()), (bad, b"x".to_vec())]),
        )
        .unwrap();
        let outer = tempfile::tempdir().unwrap();
        let dest = outer.path().join("dest");
        std::fs::create_dir(&dest).unwrap();
        let cancel = AtomicBool::new(false);
        let result = extract(
            &spec(ArchiveScheme::SplitZip, "a", vec![archive], &dest),
            &cancel,
            &mut |_| {},
        );
        assert_unsafe(&result);
        // Nothing appeared anywhere, not even the harmless first entry.
        assert!(!outer.path().join("evil.txt").exists(), "{bad}");
        assert!(!dest.join("evil.txt").exists(), "{bad}");
        assert!(!dest.join("fine.txt").exists(), "{bad}: partial extraction");
        assert_clean(&dest);

        let archive7 = src.path().join("a.7z");
        std::fs::write(&archive7, make_7z(&[(bad, b"x".to_vec())])).unwrap();
        let result = extract(
            &spec(ArchiveScheme::SevenZip, "a.7z", vec![archive7], &dest),
            &cancel,
            &mut |_| {},
        );
        assert_unsafe(&result);
        assert!(!outer.path().join("evil.txt").exists(), "{bad} (7z)");
    }
}

#[cfg(unix)]
#[test]
fn symlinks_are_skipped_not_followed() {
    let src = tempfile::tempdir().unwrap();
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    writer.add_symlink("link", "/etc/passwd", options).unwrap();
    writer.start_file("real.txt", options).unwrap();
    writer.write_all(b"real").unwrap();
    // A file written *through* the link name must not escape.
    writer.start_file("link/escaped.txt", options).unwrap();
    writer.write_all(b"x").unwrap();
    let archive = src.path().join("a.zip");
    std::fs::write(&archive, writer.finish().unwrap().into_inner()).unwrap();

    let (run, result) = run(ArchiveScheme::SplitZip, "a", vec![archive], None);
    let summary = result.unwrap();
    assert_eq!(summary.skipped_links, 1);
    let link = run.out.path().join("link");
    // No symlink was created; "link" is an ordinary directory here.
    assert!(
        !std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read(run.out.path().join("real.txt")).unwrap(),
        b"real"
    );
    assert!(!Path::new("/etc/passwd/escaped.txt").exists());
}

#[test]
fn archive_bombs_are_refused() {
    // 2 MiB of zeros compresses to a few KiB.
    let src = tempfile::tempdir().unwrap();
    let archive = src.path().join("bomb.zip");
    std::fs::write(&archive, make_zip(&[("zeros", vec![0u8; 2 << 20])])).unwrap();

    // Ratio: the floor is lowered so this small archive trips it.
    let ratio = ExtractLimits {
        max_bytes: 1 << 30,
        max_ratio: 50,
        ratio_floor: 64 * 1024,
        max_entries: 100,
    };
    let (first, result) = run(
        ArchiveScheme::SplitZip,
        "bomb",
        vec![archive.clone()],
        Some(ratio),
    );
    assert!(
        matches!(result, Err(ExtractError::TooLarge { .. })),
        "{result:?}"
    );
    assert!(!first.out.path().join("zeros").exists());
    assert_clean(first.out.path());

    // Total size, refused from the headers before writing anything.
    let total = ExtractLimits {
        max_bytes: 1 << 20,
        ..ratio
    };
    let (run, result) = run(ArchiveScheme::SplitZip, "bomb", vec![archive], Some(total));
    assert!(
        matches!(result, Err(ExtractError::TooLarge { .. })),
        "{result:?}"
    );
    assert_clean(run.out.path());
}

#[test]
fn too_many_entries_are_refused() {
    let entries: Vec<(String, Vec<u8>)> = (0..50).map(|i| (format!("f{i}"), vec![1u8])).collect();
    let borrowed: Vec<(&str, Vec<u8>)> = entries
        .iter()
        .map(|(n, d)| (n.as_str(), d.clone()))
        .collect();
    let src = tempfile::tempdir().unwrap();
    let archive = src.path().join("many.zip");
    std::fs::write(&archive, make_zip(&borrowed)).unwrap();
    let few = ExtractLimits {
        max_entries: 10,
        ..limits()
    };
    let (_run, result) = run(ArchiveScheme::SplitZip, "many", vec![archive], Some(few));
    assert!(
        matches!(result, Err(ExtractError::TooLarge { .. })),
        "{result:?}"
    );
}

#[test]
fn cancelling_leaves_nothing_behind() {
    let src = tempfile::tempdir().unwrap();
    let archive = src.path().join("a.zip");
    std::fs::write(&archive, make_zip(&files())).unwrap();
    let out = tempfile::tempdir().unwrap();
    let cancel = AtomicBool::new(false);
    let mut seen = 0;
    let result = extract(
        &spec(ArchiveScheme::SplitZip, "a", vec![archive], out.path()),
        &cancel,
        &mut |_| {
            seen += 1;
            if seen == 2 {
                cancel.store(true, Ordering::Relaxed);
            }
        },
    );
    assert!(matches!(result, Err(ExtractError::Cancelled)), "{result:?}");
    assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 0);
}

#[test]
fn unsupported_cases_say_so() {
    let src = tempfile::tempdir().unwrap();
    let a = src.path().join("a.z01");
    let b = src.path().join("a.zip");
    std::fs::write(&a, b"x").unwrap();
    std::fs::write(&b, b"y").unwrap();
    let (_run, result) = run(ArchiveScheme::SplitZip, "a", vec![a, b], None);
    assert!(
        matches!(result, Err(ExtractError::Unsupported { .. })),
        "{result:?}"
    );

    #[cfg(not(feature = "rar"))]
    {
        let rar = src.path().join("a.rar");
        std::fs::write(&rar, b"Rar!").unwrap();
        let (_run, result) = run(ArchiveScheme::Rar, "a", vec![rar], None);
        assert!(
            matches!(result, Err(ExtractError::Unsupported { .. })),
            "{result:?}"
        );
    }
}

// --- RAR (needs the `rar` feature; fixtures are hand-assembled, see
// tests/fixtures/rar/make_fixtures.py) ---

#[cfg(feature = "rar")]
mod rar {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/rar")
            .join(name)
    }

    #[test]
    fn a_single_rar_extracts() {
        let (run, result) = run(
            ArchiveScheme::RarLegacy,
            "single",
            vec![fixture("single.rar")],
            None,
        );
        result.unwrap();
        assert_eq!(
            std::fs::read(run.out.path().join("hello.txt")).unwrap(),
            b"hello from a rar archive\n"
        );
        assert_clean(run.out.path());
    }

    #[test]
    fn a_multi_volume_rar_extracts_across_volumes() {
        let (run, result) = run(
            ArchiveScheme::Rar,
            "split",
            vec![fixture("split.part1.rar"), fixture("split.part2.rar")],
            None,
        );
        result.unwrap();
        let expected: Vec<u8> = (0..3000).map(|i| ((i * 31 + 7) % 251) as u8).collect();
        assert_eq!(
            std::fs::read(run.out.path().join("big.bin")).unwrap(),
            expected
        );
    }

    #[test]
    fn rar_path_traversal_is_refused() {
        let outer = tempfile::tempdir().unwrap();
        let dest = outer.path().join("dest");
        std::fs::create_dir(&dest).unwrap();
        let cancel = AtomicBool::new(false);
        let result = extract(
            &spec(
                ArchiveScheme::RarLegacy,
                "evil",
                vec![fixture("evil.rar")],
                &dest,
            ),
            &cancel,
            &mut |_| {},
        );
        assert_unsafe(&result);
        assert!(!outer.path().join("evil.txt").exists());
    }

    #[test]
    fn a_damaged_rar_is_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = std::fs::read(fixture("single.rar")).unwrap();
        let last = bytes.len() - 20;
        bytes[last] ^= 0xFF;
        let path = dir.path().join("bad.rar");
        std::fs::write(&path, bytes).unwrap();
        let (_run, result) = run(ArchiveScheme::RarLegacy, "bad", vec![path], None);
        assert!(result.is_err());
    }
}

#[cfg(unix)]
#[test]
fn a_symlink_planted_as_the_staging_directory_is_not_followed() {
    let src = tempfile::tempdir().unwrap();
    let archive = src.path().join("a.zip");
    std::fs::write(&archive, make_zip(&files())).unwrap();
    let out = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    // The staging name is predictable; an attacker with write access to
    // the destination could aim it somewhere else.
    std::os::unix::fs::symlink(elsewhere.path(), out.path().join(".a.nuxdextract")).unwrap();

    let cancel = AtomicBool::new(false);
    extract(
        &spec(ArchiveScheme::SplitZip, "a", vec![archive], out.path()),
        &cancel,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(
        std::fs::read_dir(elsewhere.path()).unwrap().count(),
        0,
        "extraction wrote through the planted link"
    );
    assert_files(out.path());
}
