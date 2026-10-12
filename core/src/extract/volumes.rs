//! `MultiVolumeReader`: ordered volume files presented as one `Read +
//! Seek` stream (`ARCHITECTURE.md` §12.4). It serves `.7z.001…` and any
//! generic split with the same code. Only one volume is open at a time,
//! so a 500-volume set doesn't exhaust file descriptors.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;

pub struct MultiVolumeReader {
    paths: Vec<PathBuf>,
    /// Offset of each volume's first byte in the joined stream.
    starts: Vec<u64>,
    total: u64,
    position: u64,
    current: Option<(usize, File)>,
}

impl MultiVolumeReader {
    /// Opens `paths` in order. Fails if any volume can't be inspected.
    pub fn new(paths: Vec<PathBuf>) -> io::Result<Self> {
        let mut starts = Vec::with_capacity(paths.len());
        let mut total = 0u64;
        for path in &paths {
            starts.push(total);
            total += std::fs::metadata(path)?.len();
        }
        Ok(Self {
            paths,
            starts,
            total,
            position: 0,
            current: None,
        })
    }

    /// Total length of the joined stream.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.total
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// The volume holding `position` (the last one that starts at or
    /// before it).
    fn volume_at(&self, position: u64) -> usize {
        self.starts
            .partition_point(|start| *start <= position)
            .saturating_sub(1)
    }
}

impl Read for MultiVolumeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.position >= self.total {
            return Ok(0);
        }
        let index = self.volume_at(self.position);
        let offset = self.position - self.starts[index];
        let volume_len =
            self.starts.get(index + 1).copied().unwrap_or(self.total) - self.starts[index];
        if self.current.as_ref().is_none_or(|(i, _)| *i != index) {
            self.current = Some((index, File::open(&self.paths[index])?));
        }
        let Some((_, file)) = self.current.as_mut() else {
            return Ok(0);
        };
        file.seek(SeekFrom::Start(offset))?;
        let want = usize::try_from(volume_len - offset)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let n = file.read(&mut buf[..want])?;
        if n == 0 {
            // The volume is shorter than when we measured it.
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("volume {} ended early", self.paths[index].display()),
            ));
        }
        self.position += n as u64;
        Ok(n)
    }
}

impl Seek for MultiVolumeReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::End(delta) => i128::from(self.total) + i128::from(delta),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start of the stream",
            ));
        }
        self.position = u64::try_from(target).unwrap_or(u64::MAX);
        Ok(self.position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volumes(parts: &[&[u8]]) -> (tempfile::TempDir, MultiVolumeReader) {
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<PathBuf> = parts
            .iter()
            .enumerate()
            .map(|(i, bytes)| {
                let path = dir.path().join(format!("v{i}"));
                std::fs::write(&path, bytes).unwrap();
                path
            })
            .collect();
        let reader = MultiVolumeReader::new(paths).unwrap();
        (dir, reader)
    }

    #[test]
    fn reads_across_volume_boundaries() {
        let (_dir, mut r) = volumes(&[b"abc", b"defg", b"h"]);
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"abcdefgh");
        assert_eq!(r.len(), 8);
    }

    #[test]
    fn seeks_anywhere_including_backwards_and_from_the_end() {
        let (_dir, mut r) = volumes(&[b"abc", b"defg", b"h"]);
        r.seek(SeekFrom::Start(5)).unwrap();
        let mut two = [0u8; 2];
        r.read_exact(&mut two).unwrap();
        assert_eq!(&two, b"fg");
        r.seek(SeekFrom::Current(-6)).unwrap();
        r.read_exact(&mut two).unwrap();
        assert_eq!(&two, b"bc");
        assert_eq!(r.seek(SeekFrom::End(-1)).unwrap(), 7);
        let mut one = [0u8; 1];
        r.read_exact(&mut one).unwrap();
        assert_eq!(&one, b"h");
        assert_eq!(r.read(&mut one).unwrap(), 0);
    }

    #[test]
    fn empty_volumes_are_skipped_and_seeking_before_zero_fails() {
        let (_dir, mut r) = volumes(&[b"", b"ab", b"", b"c"]);
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"abc");
        assert!(r.seek(SeekFrom::Current(-10)).is_err());
    }
}
