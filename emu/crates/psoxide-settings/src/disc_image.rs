//! Disc images read on demand.
//!
//! A mounted disc used to be the whole image in memory: 700 MB for a full
//! CD, most of which a game never touches in a session. Here a track is a
//! byte range of an image that is read (or, for ECM, decoded) a sector at a
//! time when the drive asks for it. The operating system's file cache does
//! the caching for plain images; ECM keeps a few decoded chunks.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use psx_iso::TrackSource;

/// A shared, random-access disc image that tracks are cut from.
pub type SharedImage = Arc<dyn TrackSource>;

/// A raw image file, read with positioned reads so nothing of it is held
/// in process memory.
pub struct FileImage {
    file: File,
    len: u64,
}

impl FileImage {
    /// Open `path` for reading.
    pub fn open(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let len = file
            .metadata()
            .map_err(|e| format!("{}: {e}", path.display()))?
            .len();
        Ok(Self { file, len })
    }
}

impl TrackSource for FileImage {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> bool {
        if offset
            .checked_add(out.len() as u64)
            .is_none_or(|end| end > self.len)
        {
            return false;
        }
        read_exact_at(&self.file, offset, out)
    }
}

#[cfg(unix)]
fn read_exact_at(file: &File, offset: u64, out: &mut [u8]) -> bool {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(out, offset).is_ok()
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut offset: u64, mut out: &mut [u8]) -> bool {
    use std::os::windows::fs::FileExt;
    while !out.is_empty() {
        match file.seek_read(out, offset) {
            Ok(0) | Err(_) => return false,
            Ok(n) => {
                out = &mut out[n..];
                offset += n as u64;
            }
        }
    }
    true
}

#[cfg(not(any(unix, windows)))]
fn read_exact_at(_file: &File, _offset: u64, _out: &mut [u8]) -> bool {
    false
}

/// One track's bytes: the `len` bytes at `base` of a shared image. Several
/// tracks of a single-file CUE or a CloneCD image share one open file.
pub struct ImageSlice {
    image: SharedImage,
    base: u64,
    len: u64,
}

impl ImageSlice {
    /// The `len` bytes at `base` of `image`; clamped to the image.
    pub fn new(image: SharedImage, base: u64, len: u64) -> Self {
        let base = base.min(image.len());
        let len = len.min(image.len() - base);
        Self { image, base, len }
    }

    /// Byte `offset` of this slice as a range of the image, if it fits.
    fn span(&self, offset: u64, len: u64) -> Option<u64> {
        let end = offset.checked_add(len)?;
        (end <= self.len).then_some(self.base + offset)
    }
}

impl TrackSource for ImageSlice {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> bool {
        self.span(offset, out.len() as u64)
            .is_some_and(|at| self.image.read_at(at, out))
    }

    fn ready(&self, offset: u64, len: u64) -> bool {
        self.span(offset, len)
            .is_none_or(|at| self.image.ready(at, len))
    }

    fn prefetch(&self, offset: u64, len: u64) {
        let len = len.min(self.len.saturating_sub(offset));
        if let Some(at) = self.span(offset, len) {
            self.image.prefetch(at, len);
        }
    }
}

/// Open a raw image, or an ECM-packed one (decoded on demand), by content:
/// ECM streams start with `ECM\0`.
pub fn open_image(path: &Path) -> Result<SharedImage, String> {
    let file = FileImage::open(path)?;
    let mut magic = [0u8; 4];
    if file.read_at(0, &mut magic) && &magic == b"ECM\0" {
        return open_ecm(path);
    }
    Ok(Arc::new(file))
}

/// Open an ECM-packed image, decoded on demand.
pub fn open_ecm(path: &Path) -> Result<SharedImage, String> {
    let file = FileImage::open(path)?;
    let image = crate::ecm::EcmImage::open(Arc::new(file))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Arc::new(image))
}

/// Read all of a track source in order through `f`, a megabyte at a time,
/// without holding more than that. Stops early and returns `false` if a read
/// fails.
pub fn for_each_chunk(source: &dyn TrackSource, mut f: impl FnMut(&[u8])) -> bool {
    const CHUNK: usize = 1 << 20;
    let mut buf = vec![0u8; CHUNK.min(source.len() as usize)];
    let mut at = 0u64;
    while at < source.len() {
        let n = CHUNK.min((source.len() - at) as usize);
        if !source.read_at(at, &mut buf[..n]) {
            return false;
        }
        f(&buf[..n]);
        at += n as u64;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_reads_stay_inside_their_range() {
        let image: SharedImage = Arc::new((0..100u8).collect::<Vec<u8>>());
        let slice = ImageSlice::new(image, 10, 20);
        let mut out = [0u8; 5];
        assert!(slice.read_at(15, &mut out));
        assert_eq!(out, [25, 26, 27, 28, 29]);
        assert!(!slice.read_at(16, &mut out));
        assert_eq!(slice.len(), 20);
    }

    #[test]
    fn slice_is_clamped_to_the_image() {
        let image: SharedImage = Arc::new(vec![7u8; 10]);
        let slice = ImageSlice::new(image.clone(), 8, 50);
        assert_eq!(slice.len(), 2);
        let slice = ImageSlice::new(image, 50, 50);
        assert_eq!(slice.len(), 0);
    }

    #[test]
    fn file_image_reads_positioned_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.bin");
        std::fs::write(&path, (0..=255u8).collect::<Vec<u8>>()).unwrap();
        let image = FileImage::open(&path).unwrap();
        assert_eq!(image.len(), 256);
        let mut out = [0u8; 4];
        assert!(image.read_at(252, &mut out));
        assert_eq!(out, [252, 253, 254, 255]);
        assert!(!image.read_at(253, &mut out));
    }

    #[test]
    fn chunks_cover_the_whole_source_in_order() {
        let bytes: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let mut seen = Vec::new();
        assert!(for_each_chunk(&bytes, |c| seen.extend_from_slice(c)));
        assert_eq!(seen, bytes);
    }
}
