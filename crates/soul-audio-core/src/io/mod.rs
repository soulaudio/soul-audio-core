//! No-alloc I/O plumbing for decoders.
//!
//! Decoders read bytes through a **capability ladder** of owned traits, each
//! adding one power to the one below:
//!
//! - [`Source`] — sequential byte input (the universal baseline).
//! - [`BufSource`] — [`Source`] + peek-without-consume (cheap header parsing).
//! - [`SeekSource`] — [`Source`] + random access (playback seeking binds here).
//!
//! They are thin, blanket-implemented super-traits over the corresponding
//! [`embedded_io`] traits, so anything that is `embedded_io::Read` (etc.)
//! satisfies them automatically — while decoders and callers speak this crate's
//! vocabulary rather than `embedded-io`'s directly.
//!
//! Two concrete, no-alloc sources ship here: [`SliceSource`] (an in-memory slice
//! — its own buffer, so it climbs the whole ladder) and [`BufReader`] (adds
//! [`BufSource`] to any [`Source`] via a caller-owned buffer). Platform backends
//! — a desktop `File`/`TcpStream`, an embedded SD card — live in the *shells*
//! and reach the ladder through `embedded-io` / `embedded-io-adapters`.

mod buf_reader;
mod slice;
mod source;

pub use buf_reader::BufReader;
pub use slice::SliceSource;
pub use source::{BufSource, SeekSource, Source};
