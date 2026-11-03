//! CELT layer of the from-scratch Opus decoder (RFC 6716 §4.3).
//!
//! CELT (Constrained Energy Lapped Transform) is the MDCT-based half of Opus,
//! used standalone for music and as the high band of the hybrid mode. This
//! module is built up over several tasks; the static tables every stage reads —
//! the 21-band [`super::opus_tables`] energy-band layout, the bit-allocation
//! matrix and pulse cache, the coarse-energy Laplace models, and the MDCT
//! overlap window — are generated host-side by `cargo xtask gen-opus-tables`
//! (so the `no_std` core computes no trig/combinatorics at runtime).
//!
//! [`mdct`] holds the overlap window today; the inverse-MDCT synthesis itself
//! lands in a later task (see that module's docs). [`energy`] decodes the
//! per-frame preamble and the coarse/fine/finalise band-energy envelope
//! (RFC 6716 §4.3.1–4.3.3).

pub mod allocation;
pub mod bands;
pub mod cwrs;
pub mod decoder;
pub mod energy;
pub mod mdct;
pub mod vq;

// The per-band normalised-shape decode (`vq::alg_unquant`) is the
// `decode_band_shape` primitive the design spec names; the whole-frame
// orchestrator is `bands::quant_all_bands`. Both are consumed by the Task-8
// CELT frame driver (`celt_decode_with_ec`) together with `vq::renormalise_vector`
// and `bands::celt_lcg_rand` (anti-collapse). Re-exported here so Task 8 has a
// single `celt::` surface.
#[allow(unused_imports)]
pub(crate) use bands::quant_all_bands;
#[allow(unused_imports)]
pub(crate) use vq::alg_unquant as decode_band_shape;
