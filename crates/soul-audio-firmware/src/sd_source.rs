//! SD-card byte source — **firmware only**.
//!
//! Maps an open [`embedded_sdmmc::File`] onto the core's capability ladder
//! (`Source` + `SeekSource`) through `embedded-io`, so the *same* generic
//! `soul-audio-core` decode path that runs over a desktop `File` or an in-flash
//! `SliceSource` also runs over an SD card.
//!
//! This deliberately lives in the firmware crate, **never** in the core library:
//! the core stays `no_std` + `embedded-io`-only and must not depend on
//! `embedded-sdmmc` or any hardware stack.
//!
//! Constructing an [`SdSource`] needs the board's SPI/`SdCard`/`VolumeManager`
//! wiring (board-specific — part of the SAI/DMA firmware work). The adapter
//! itself is complete: an SD file is a full-ladder source, exactly like a
//! desktop `File`.

use embedded_io::{ErrorKind, ErrorType, Read, Seek, SeekFrom};
use embedded_sdmmc::{BlockDevice, File, TimeSource};

/// Opaque SD error, surfaced to `embedded-io` as [`ErrorKind::Other`].
#[derive(Debug)]
pub struct SdError;

impl embedded_io::Error for SdError {
    fn kind(&self) -> ErrorKind {
        ErrorKind::Other
    }
}

/// An `embedded-io` source over an open `embedded-sdmmc` [`File`]. Reads stream
/// forward (EOF is `Ok(0)`); seeks give random access. Tracks the byte offset so
/// [`Seek`] can report the new absolute position.
#[allow(dead_code)] // constructed once the board SD stack (SPI/VolumeManager) is wired
pub struct SdSource<
    'a,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    D: BlockDevice,
    T: TimeSource,
{
    file: File<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    pos: u32,
}

impl<'a, D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>
    SdSource<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: BlockDevice,
    T: TimeSource,
{
    /// Wrap an open file, starting at offset 0.
    #[allow(dead_code)]
    pub fn new(file: File<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>) -> Self {
        Self { file, pos: 0 }
    }
}

impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize> ErrorType
    for SdSource<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: BlockDevice,
    T: TimeSource,
{
    type Error = SdError;
}

impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize> Read
    for SdSource<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: BlockDevice,
    T: TimeSource,
{
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        let n = self.file.read(buf).map_err(|_| SdError)?;
        self.pos += n as u32;
        Ok(n)
    }
}

impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize> Seek
    for SdSource<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: BlockDevice,
    T: TimeSource,
{
    fn seek(&mut self, pos: SeekFrom) -> Result<u64, Self::Error> {
        match pos {
            SeekFrom::Start(n) => {
                let n = n as u32;
                self.file.seek_from_start(n).map_err(|_| SdError)?;
                self.pos = n;
            }
            SeekFrom::Current(off) => {
                self.file
                    .seek_from_current(off as i32)
                    .map_err(|_| SdError)?;
                self.pos = (i64::from(self.pos) + off) as u32;
            }
            SeekFrom::End(off) => {
                // `off` is bytes back from the end (<= 0 in normal use).
                let back = (-off) as u32;
                self.file.seek_from_end(back).map_err(|_| SdError)?;
                self.pos = self.file.length().saturating_sub(back);
            }
        }
        Ok(u64::from(self.pos))
    }
}
