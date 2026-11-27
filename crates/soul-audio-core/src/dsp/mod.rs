//! The **DSP chain**: an ordered, individually-bypassable set of stages the
//! deck runs on each output block, after decode/resample/downmix and before the
//! sink.
//!
//! The chain is the spine the individual DSP features plug into — the
//! parametric EQ today, crossfeed and a convolver next. Three properties shape
//! the design:
//!
//! - **Bypass-all is bit-exact.** With every stage off (or the master bypass
//!   on), [`Chain::is_active`] is `false`, no stage runs, and the output is
//!   the identical passthrough the bit-perfect claim
//!   ([`DecodeInfo::bit_perfect`](crate::deck::DecodeInfo::bit_perfect)) rests
//!   on; any active stage flips that claim to `false`. The deck skips the whole
//!   chain — including the `i32`→`f32`→`i32` conversion around it — while
//!   inactive. (That conversion is itself lossless for canonical samples: a
//!   24-bit integer is exact in `f32`. Skipping it saves the work, and means
//!   adding a stage cannot change the audio until it is configured to.)
//! - **`no_std`, no-alloc.** Fixed stage capacity ([`MAX_STAGES`]), fixed band
//!   counts, no trait objects: stages are an enum
//!   ([`Stage`]) dispatched like [`AnyDecoder`](crate::dispatch::AnyDecoder), so
//!   the whole chain is one inline value with no heap and no `dyn`.
//! - **Per output.** The chain lives on the [`Deck`](crate::deck::Deck), and a
//!   deck is one output, so each device/zone configures its own independently.
//!
//! Stages process **interleaved `f32`** at the *sink* rate (the deck converts
//! to and from its canonical `i32` samples at the call site, clamping on the way
//! back). The coefficient *design* — the transcendental half of an EQ — lives
//! outside core (`soul-audio-eq`), exactly as it does for
//! [`Equalizer`](crate::eq::Equalizer) itself.

pub mod convolve;
pub mod crossfeed;

pub use convolve::Convolver;
pub use crossfeed::CrossfeedStage;

use crate::eq::Equalizer;

/// How many stages one chain can hold. Four covers the planned set (EQ,
/// crossfeed, convolver) with a slot spare; pushing past it is refused rather
/// than allocating.
pub const MAX_STAGES: usize = 4;

/// Parametric bands the chain's EQ stage carries. Matches `soul_audio_eq`'s
/// `MAX_BANDS`, which is what the desktop host and the DAP both design against
/// (core cannot depend on that crate — it depends on core).
pub const EQ_BANDS: usize = 16;

/// The default [`Chain`] storage: **none**.
///
/// A chain is parameterized by the storage its convolution stage needs
/// ([`convolve::Convolver`]), because a measured impulse response is caller
/// memory — a megabyte of it — and a chain slot is an inline value. Most decks
/// never host a convolver, so the default is this zero-sized stand-in: it
/// presents an empty slice, which means
/// [`Convolver::set_impulse_response`](convolve::Convolver::set_impulse_response)
/// on a default-storage chain declines with
/// [`ConvolveError::StorageTooSmall`](convolve::ConvolveError::StorageTooSmall)
/// rather than pretending to work.
///
/// A host that wants room correction names its storage instead —
/// `Deck<Sink, Dec, PRELOAD, Vec<f32>>` on the desktop, a `StaticCell` slice on
/// the DAP — and every existing `Deck<Sink, Dec, PRELOAD>` keeps compiling
/// unchanged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoStorage;

impl AsRef<[f32]> for NoStorage {
    fn as_ref(&self) -> &[f32] {
        &[]
    }
}

impl AsMut<[f32]> for NoStorage {
    fn as_mut(&mut self) -> &mut [f32] {
        &mut []
    }
}

/// What a stage *is*, for the signal-path UI — "which stages are active".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StageKind {
    /// Parametric EQ (a biquad band cascade).
    Eq,
    /// Headphone crossfeed (see [`crossfeed`]).
    Crossfeed,
    /// Impulse-response convolution — room / headphone correction (see
    /// [`convolve`]). [`Convolver`] is generic over its (caller-provided)
    /// filter storage, so it is not a [`Stage`] variant yet — see that
    /// module's "How a host runs it". The kind exists so a signal path can
    /// name the stage wherever it runs.
    Convolver,
}

/// One block-processing stage. Implemented by each concrete stage and forwarded
/// by [`Stage`]; the trait exists so a stage can be exercised (and a new one
/// written) without going through the enum or the deck.
pub trait DspStage {
    /// Process `buf` in place: `channels`-interleaved `f32` samples at
    /// `sample_rate` (the sink's rate — stages run post-resample). Must be a
    /// no-op when [`is_active`](DspStage::is_active) is `false`.
    fn process(&mut self, buf: &mut [f32], channels: usize, sample_rate: u32);

    /// Whether this stage would change the samples. A stage that is disabled,
    /// or enabled but configured flat, is **not** active: it must neither cost
    /// the chain its bit-exact bypass nor the path its bit-perfect claim.
    fn is_active(&self) -> bool;

    /// Clear any internal state (filter delay lines, buffers) — called on a
    /// playback discontinuity such as a seek, so stale state can't smear
    /// across the splice.
    fn reset(&mut self);

    /// What kind of stage this is.
    fn kind(&self) -> StageKind;
}

/// The parametric-EQ stage: a [`Equalizer`] band cascade, which is already the
/// real-time primitive core applies. The host/firmware designs coefficients
/// (`soul-audio-eq`) and sets them through [`Self::equalizer_mut`].
#[derive(Clone, Debug)]
pub struct EqStage {
    eq: Equalizer<EQ_BANDS>,
}

impl Default for EqStage {
    fn default() -> Self {
        Self::new()
    }
}

impl EqStage {
    /// A flat (all-bypass) EQ stage, enabled: it is active only once a band is
    /// set to something other than a passthrough.
    #[must_use]
    pub fn new() -> Self {
        Self {
            eq: Equalizer::new(),
        }
    }

    /// The underlying equalizer, to set bands / toggle it.
    pub fn equalizer_mut(&mut self) -> &mut Equalizer<EQ_BANDS> {
        &mut self.eq
    }

    /// The underlying equalizer.
    #[must_use]
    pub fn equalizer(&self) -> &Equalizer<EQ_BANDS> {
        &self.eq
    }
}

impl DspStage for EqStage {
    fn process(&mut self, buf: &mut [f32], channels: usize, _sample_rate: u32) {
        // Biquad coefficients are designed for a fixed rate by the caller, so
        // the stage itself is rate-agnostic.
        self.eq.process(buf, channels);
    }

    fn is_active(&self) -> bool {
        self.eq.is_active()
    }

    fn reset(&mut self) {
        self.eq.reset();
    }

    fn kind(&self) -> StageKind {
        StageKind::Eq
    }
}

/// A chain slot: one concrete stage. An enum rather than a `dyn DspStage` so
/// the chain needs neither allocation nor indirection.
///
/// The variants differ a lot in size — an EQ carries 16 bands' coefficients
/// plus per-channel delay lines (~2.4 KiB), a crossfeed carries two filter
/// states — and that is accepted deliberately: boxing the big one is exactly
/// what a no-alloc crate cannot do, and the slot count is fixed and tiny
/// ([`MAX_STAGES`]), so the worst case is a few kilobytes of inline storage
/// sized for the largest stage. Same tradeoff (and the same `allow`) as
/// [`AnyDecoder`](crate::dispatch::AnyDecoder) and
/// [`MatroskaDecoder`](crate::formats::matroska::MatroskaDecoder).
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum Stage<P = NoStorage> {
    /// Parametric EQ.
    Eq(EqStage),
    /// Headphone crossfeed.
    Crossfeed(CrossfeedStage),
    /// Impulse-response convolution (room / headphone correction) over
    /// caller-provided filter storage `P` — see [`NoStorage`] for why the
    /// chain is parameterized by it.
    Convolver(Convolver<P>),
}

impl<P: AsRef<[f32]> + AsMut<[f32]>> DspStage for Stage<P> {
    fn process(&mut self, buf: &mut [f32], channels: usize, sample_rate: u32) {
        match self {
            Stage::Eq(s) => s.process(buf, channels, sample_rate),
            Stage::Crossfeed(s) => s.process(buf, channels, sample_rate),
            Stage::Convolver(s) => s.process(buf, channels, sample_rate),
        }
    }

    fn is_active(&self) -> bool {
        match self {
            Stage::Eq(s) => s.is_active(),
            Stage::Crossfeed(s) => s.is_active(),
            Stage::Convolver(s) => s.is_active(),
        }
    }

    fn reset(&mut self) {
        match self {
            Stage::Eq(s) => s.reset(),
            Stage::Crossfeed(s) => s.reset(),
            Stage::Convolver(s) => s.reset(),
        }
    }

    fn kind(&self) -> StageKind {
        match self {
            Stage::Eq(s) => s.kind(),
            Stage::Crossfeed(s) => s.kind(),
            Stage::Convolver(s) => s.kind(),
        }
    }
}

/// An ordered chain of up to [`MAX_STAGES`] stages, with a master bypass.
///
/// Deliberately named `dsp::Chain`, not `DspChain`: `soul-audio-dsp` already
/// owns a `DspChain` — the portable **configuration** model (named profiles,
/// serde exchange with the DAP, `alloc`-backed `Vec`s of bands). This is the
/// **runtime** chain the deck actually runs, fixed-capacity and no-alloc. A
/// head turns the former into the latter: design each unit's bands and push the
/// stages here.
///
/// Default: empty, not bypassed — which is inactive, so a deck that never
/// configures DSP behaves exactly as it did before the chain existed.
#[derive(Clone, Debug)]
pub struct Chain<P = NoStorage> {
    stages: [Option<Stage<P>>; MAX_STAGES],
    len: usize,
    bypass_all: bool,
}

impl<P> Default for Chain<P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P> Chain<P> {
    /// An empty chain.
    ///
    /// Deliberately not a `Default` bound on `P`: a DAP's storage is a
    /// `&'static mut [f32]` carved from a `StaticCell`, which has no default,
    /// and a deck must still be constructible before any filter is installed.
    #[must_use]
    pub fn new() -> Self {
        Self {
            stages: core::array::from_fn(|_| None),
            len: 0,
            bypass_all: false,
        }
    }

    /// Append `stage`. Returns `false` (leaving the chain untouched) when full
    /// — no-alloc means a hard capacity, not a silent reallocation.
    pub fn push(&mut self, stage: Stage<P>) -> bool {
        if self.len >= MAX_STAGES {
            return false;
        }
        self.stages[self.len] = Some(stage);
        self.len += 1;
        true
    }

    /// Remove the stage at `index`, shifting the rest down to keep the order
    /// contiguous. `None` if out of range.
    pub fn remove(&mut self, index: usize) -> Option<Stage<P>> {
        if index >= self.len {
            return None;
        }
        let taken = self.stages[index].take();
        for i in index + 1..self.len {
            self.stages[i - 1] = self.stages[i].take();
        }
        self.len -= 1;
        taken
    }

    /// Swap two stages — the reorder primitive (EQ-then-crossfeed vs the
    /// reverse are different sounds). `false` if either index is out of range.
    pub fn swap(&mut self, a: usize, b: usize) -> bool {
        if a >= self.len || b >= self.len {
            return false;
        }
        self.stages.swap(a, b);
        true
    }

    /// Drop every stage.
    pub fn clear(&mut self) {
        for slot in &mut self.stages {
            *slot = None;
        }
        self.len = 0;
    }

    /// How many stages the chain holds (active or not).
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the chain holds no stages at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The stage at `index`, in chain order.
    #[must_use]
    pub fn stage(&self, index: usize) -> Option<&Stage<P>> {
        self.stages.get(index)?.as_ref()
    }

    /// The stage at `index`, mutably — how a caller configures it (set EQ
    /// bands, pick a crossfeed preset).
    pub fn stage_mut(&mut self, index: usize) -> Option<&mut Stage<P>> {
        self.stages.get_mut(index)?.as_mut()
    }

    /// Bypass the **whole** chain in one move, leaving every stage's
    /// configuration and state intact — the A/B-against-untouched-audio
    /// control. A bypassed chain is inactive, hence bit-exact.
    pub fn set_bypass_all(&mut self, on: bool) {
        self.bypass_all = on;
    }

    /// Whether the master bypass is engaged.
    #[must_use]
    pub fn is_bypassed(&self) -> bool {
        self.bypass_all
    }

    /// Whether the master bypass is engaged.
    #[must_use]
    pub fn is_bypassed_marker(&self) -> bool {
        self.bypass_all
    }
}

impl<P: AsRef<[f32]> + AsMut<[f32]>> Chain<P> {
    /// Whether the chain would change the samples: not master-bypassed, and at
    /// least one stage active. The deck skips the chain entirely when this is
    /// `false`, which is what keeps bypass bit-exact.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.bypass_all && self.stages.iter().flatten().any(DspStage::is_active)
    }

    /// Write the kinds of the currently-active stages into `out`, in chain
    /// order, returning how many were written — the signal-path UI's "which
    /// stages are active". Nothing is written when the chain is inactive
    /// (including when the master bypass is on, however many stages it holds).
    pub fn active_kinds(&self, out: &mut [StageKind]) -> usize {
        if self.bypass_all {
            return 0;
        }
        let mut n = 0;
        for stage in self.stages.iter().flatten() {
            if stage.is_active() && n < out.len() {
                out[n] = stage.kind();
                n += 1;
            }
        }
        n
    }

    /// Run every active stage over `buf`, in order. A no-op when the chain is
    /// inactive, so a caller that forgets to check [`is_active`](Self::is_active)
    /// still gets a passthrough (it just pays for the conversion around it).
    pub fn process(&mut self, buf: &mut [f32], channels: usize, sample_rate: u32) {
        if self.bypass_all {
            return;
        }
        for stage in self.stages.iter_mut().flatten() {
            if stage.is_active() {
                stage.process(buf, channels, sample_rate);
            }
        }
    }

    /// Reset every stage's internal state (on a seek / fresh load).
    pub fn reset(&mut self) {
        for stage in self.stages.iter_mut().flatten() {
            stage.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eq::Biquad;

    /// A +6 dB-ish peaking-shaped section — any non-bypass coefficients make
    /// the stage active and audibly change the block.
    fn non_flat() -> Biquad {
        Biquad::new(1.2, -0.4, 0.1, -0.3, 0.05)
    }

    /// An EQ stage for a chain of any storage type (the EQ needs none).
    fn eq_stage<P>(active: bool) -> Stage<P> {
        let mut s = EqStage::new();
        if active {
            s.equalizer_mut().set_band(0, non_flat());
        }
        Stage::Eq(s)
    }

    #[test]
    fn an_empty_or_flat_chain_is_inactive() {
        let mut chain: Chain = Chain::new();
        assert!(!chain.is_active(), "an empty chain is inactive");
        assert!(chain.is_empty());

        // A stage present but configured flat is not active either.
        assert!(chain.push(eq_stage(false)));
        assert_eq!(chain.len(), 1);
        assert!(!chain.is_active(), "a flat EQ stage is not active");

        // And a flat chain leaves a block untouched.
        let mut buf = [1.0f32, -2.0, 3.0, -4.0];
        let before = buf;
        chain.process(&mut buf, 2, 48_000);
        assert_eq!(buf, before);
    }

    #[test]
    fn a_configured_stage_is_active_and_changes_the_block() {
        let mut chain: Chain = Chain::new();
        chain.push(eq_stage(true));
        assert!(chain.is_active());
        let mut buf = [1000.0f32, 1000.0, 1000.0, 1000.0];
        chain.process(&mut buf, 2, 48_000);
        assert!(
            buf.iter().any(|&s| (s - 1000.0).abs() > 1.0),
            "an active stage must change the samples: {buf:?}"
        );
    }

    #[test]
    fn master_bypass_is_inactive_and_a_passthrough() {
        let mut chain: Chain = Chain::new();
        chain.push(eq_stage(true));
        chain.set_bypass_all(true);
        assert!(chain.is_bypassed());
        assert!(!chain.is_active(), "bypass-all means inactive");
        let mut buf = [1000.0f32; 8];
        let before = buf;
        chain.process(&mut buf, 2, 48_000);
        assert_eq!(buf, before, "a bypassed chain must not touch the block");
        // Un-bypassing restores the configured chain (nothing was discarded).
        chain.set_bypass_all(false);
        assert!(chain.is_active());
    }

    #[test]
    fn capacity_is_hard_and_refuses_rather_than_allocating() {
        let mut chain: Chain = Chain::new();
        for i in 0..MAX_STAGES {
            assert!(chain.push(eq_stage(false)), "stage {i} should fit");
        }
        assert_eq!(chain.len(), MAX_STAGES);
        assert!(
            !chain.push(eq_stage(false)),
            "past capacity must be refused"
        );
        assert_eq!(chain.len(), MAX_STAGES, "a refused push changes nothing");
    }

    #[test]
    fn stages_can_be_reordered_and_removed_keeping_the_order_contiguous() {
        let mut chain: Chain = Chain::new();
        // Three distinguishable stages: only the middle one is active.
        chain.push(eq_stage(false));
        chain.push(eq_stage(true));
        chain.push(eq_stage(false));
        assert!(chain.stage(1).unwrap().is_active());

        assert!(chain.swap(0, 1));
        assert!(chain.stage(0).unwrap().is_active(), "swapped to the front");
        assert!(!chain.stage(1).unwrap().is_active());
        assert!(!chain.swap(0, 9), "out-of-range swap is refused");

        // Removing the front shifts the rest down with no hole left behind.
        let removed = chain.remove(0).expect("removed");
        assert!(removed.is_active());
        assert_eq!(chain.len(), 2);
        assert!(chain.stage(0).is_some() && chain.stage(1).is_some());
        assert!(chain.stage(2).is_none());
        assert!(chain.remove(5).is_none());

        chain.clear();
        assert!(chain.is_empty() && !chain.is_active());
    }

    /// Two *different* stage kinds in one chain run in order and are reported
    /// in order — the ordering the issue calls for (EQ-then-crossfeed and the
    /// reverse are different sounds), and the chain's reason to exist.
    #[test]
    fn mixed_stage_kinds_run_and_report_in_chain_order() {
        let mut crossfeed = crate::dsp::CrossfeedStage::new();
        crossfeed.set_preset(crate::dsp::crossfeed::Preset::Moderate);
        let mut chain: Chain = Chain::new();
        assert!(chain.push(eq_stage(true)));
        assert!(chain.push(Stage::Crossfeed(crossfeed)));
        assert!(chain.is_active());

        let mut kinds = [StageKind::Eq; MAX_STAGES];
        assert_eq!(chain.active_kinds(&mut kinds), 2);
        assert_eq!(kinds[..2], [StageKind::Eq, StageKind::Crossfeed]);

        // Order matters: the same two stages swapped produce different audio
        // (the EQ is non-linear-phase, the crossfeed is cross-channel).
        let signal: Vec<f32> = (0..512)
            .map(|i| if i % 2 == 0 { 0.5 } else { -0.25 })
            .collect();
        let mut forward = signal.clone();
        chain.process(&mut forward, 2, 48_000);

        let mut swapped: Chain = Chain::new();
        swapped.push(Stage::Crossfeed(crossfeed));
        swapped.push(eq_stage(true));
        let mut reverse = signal.clone();
        swapped.process(&mut reverse, 2, 48_000);
        assert_ne!(forward, reverse, "stage order must affect the result");

        // And the master bypass still hides both.
        chain.set_bypass_all(true);
        assert!(!chain.is_active());
        assert_eq!(chain.active_kinds(&mut kinds), 0);
        let mut untouched = signal.clone();
        chain.process(&mut untouched, 2, 48_000);
        assert_eq!(untouched, signal);
    }

    /// The convolver as a **chain stage**, over caller-provided storage: it
    /// runs in order with the others, is reported in the active set, and its
    /// bypass stays bit-exact. This is what the storage type parameter buys —
    /// a `Deck<_, _, N, Vec<f32>>` can host room correction alongside the EQ
    /// and reorder the two.
    #[test]
    fn a_convolver_runs_in_the_chain_over_caller_storage() {
        use crate::dsp::convolve::{Convolver, storage_len};

        // A short, non-trivial IR on the direct path (no latency), so the
        // comparison is straightforward.
        let ir = [0.5f32, 0.25, -0.125, 0.0625];
        let mut conv = Convolver::new(vec![0.0f32; storage_len(ir.len(), 2)]);
        conv.set_impulse_response(&ir, 2).expect("install IR");
        assert!(conv.is_active());

        let mut chain: Chain<Vec<f32>> = Chain::new();
        assert!(chain.push(Stage::Convolver(conv.clone())));
        assert!(
            chain.is_active(),
            "a loaded convolver makes the chain active"
        );
        let mut kinds = [StageKind::Eq; MAX_STAGES];
        assert_eq!(chain.active_kinds(&mut kinds), 1);
        assert_eq!(kinds[0], StageKind::Convolver);

        let signal: Vec<f32> = (0..256).map(|i| ((i % 17) as f32 - 8.0) / 8.0).collect();
        let mut convolved = signal.clone();
        chain.process(&mut convolved, 2, 48_000);
        assert_ne!(convolved, signal, "the convolver must process the block");

        // Bypassing the chain restores the exact samples, as for every stage.
        chain.set_bypass_all(true);
        let mut bypassed = signal.clone();
        chain.process(&mut bypassed, 2, 48_000);
        assert_eq!(bypassed, signal, "a bypassed convolver is bit-exact");

        // Order matters against the EQ, which is the point of having a chain:
        // convolve-then-EQ differs from EQ-then-convolve.
        let build = |convolver_first: bool| {
            let mut c: Chain<Vec<f32>> = Chain::new();
            if convolver_first {
                c.push(Stage::Convolver(conv.clone()));
                c.push(eq_stage(true));
            } else {
                c.push(eq_stage(true));
                c.push(Stage::Convolver(conv.clone()));
            }
            let mut buf = signal.clone();
            c.process(&mut buf, 2, 48_000);
            buf
        };
        assert_ne!(
            build(true),
            build(false),
            "stage order must change the result"
        );
    }

    /// The default storage is honest about being none: a convolver on a
    /// default-parameterized chain cannot take a filter, so it stays inactive
    /// and the chain stays a passthrough — rather than silently truncating
    /// someone's room correction.
    #[test]
    fn the_default_storage_declines_a_filter_instead_of_pretending() {
        use crate::dsp::convolve::{ConvolveError, Convolver};

        let mut conv: Convolver<NoStorage> = Convolver::new(NoStorage);
        let ir = [0.5f32; 8];
        assert!(matches!(
            conv.set_impulse_response(&ir, 2),
            Err(ConvolveError::StorageTooSmall { .. })
        ));
        assert!(!conv.is_active());

        let mut chain: Chain = Chain::new();
        assert!(chain.push(Stage::Convolver(conv)));
        assert!(!chain.is_active(), "no filter, no processing");
        let signal: Vec<f32> = (0..64).map(|i| i as f32).collect();
        let mut buf = signal.clone();
        chain.process(&mut buf, 2, 48_000);
        assert_eq!(buf, signal);
    }

    #[test]
    fn active_kinds_reports_only_the_active_stages_in_order() {
        let mut chain: Chain = Chain::new();
        chain.push(eq_stage(false));
        chain.push(eq_stage(true));
        let mut kinds = [StageKind::Eq; MAX_STAGES];
        assert_eq!(chain.active_kinds(&mut kinds), 1);
        assert_eq!(kinds[0], StageKind::Eq);
        // The master bypass hides everything, however much is configured.
        chain.set_bypass_all(true);
        assert_eq!(chain.active_kinds(&mut kinds), 0);
        // A buffer smaller than the active count is filled, not overrun.
        chain.set_bypass_all(false);
        chain.push(eq_stage(true));
        assert_eq!(chain.active_kinds(&mut []), 0);
        let mut one = [StageKind::Eq; 1];
        assert_eq!(chain.active_kinds(&mut one), 1);
    }

    #[test]
    fn reset_clears_filter_state_so_two_runs_match() {
        let mut chain: Chain = Chain::new();
        chain.push(eq_stage(true));
        let input = [1000.0f32, -1000.0, 500.0, 0.0, 250.0, 125.0];
        let mut first = input;
        chain.process(&mut first, 2, 48_000);
        // Without a reset the IIR state carries over, so the same input gives
        // a different answer; after one it is reproducible.
        let mut again = input;
        chain.process(&mut again, 2, 48_000);
        assert_ne!(first, again, "IIR state carries across blocks (as it must)");
        chain.reset();
        let mut third = input;
        chain.process(&mut third, 2, 48_000);
        assert_eq!(first, third, "reset must restore the initial state");
    }
}
