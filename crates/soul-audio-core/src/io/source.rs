//! The capability-ladder traits: [`Source`] → [`BufSource`] → [`SeekSource`].
//!
//! Each is a thin super-trait, blanket-implemented over the corresponding
//! [`embedded_io`] trait, so any `embedded_io::Read`/`BufRead`/`Seek` type is one
//! automatically.

use embedded_io::{BufRead, Read, Seek};

/// Sequential byte input — the universal baseline. Every backend is a `Source`
/// (in-flash slice, SD-card file, desktop `File`, a socket).
///
/// A decoder that bounds on `Source` streams forward and therefore works over
/// *anything*, including a non-seekable stream.
pub trait Source: Read {}
impl<T: Read + ?Sized> Source for T {}

/// A [`Source`] you can peek at without consuming, via an internal buffer
/// ([`fill_buf`](embedded_io::BufRead::fill_buf) /
/// [`consume`](embedded_io::BufRead::consume)). Makes container-header and
/// frame-sync parsing cheap. Obtain one from a [`SliceSource`](crate::SliceSource)
/// (its own buffer) or by wrapping any `Source` in a
/// [`BufReader`](crate::BufReader) with a caller-owned buffer.
pub trait BufSource: Source + BufRead {}
impl<T: Read + BufRead + ?Sized> BufSource for T {}

/// A [`Source`] with random access. **Playback seeking binds here**, so the type
/// system forbids seeking a source that cannot seek (e.g. a live stream).
///
/// Backed by slices and files (incl. SD cards); never by a stream.
pub trait SeekSource: Source + Seek {}
impl<T: Read + Seek + ?Sized> SeekSource for T {}
