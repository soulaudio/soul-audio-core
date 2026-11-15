//! Portable **DSP-chain + audio-profile** model, shared by the desktop host
//! (`soul-player-ng`) and the DAP firmware (`soul-listener`).
//!
//! A [`DspChain`] is an ordered list of typed [`DspUnit`]s (EQ is the only kind
//! today) the signal flows through. A [`Profile`] is a named snapshot of one chain,
//! and a [`ProfileSet`] holds several with one active — plus the pure switching /
//! management logic (`switch`, `add`, `duplicate`, `remove`, `rename`), free of any
//! I/O or engine handle so **both heads reuse it identically**.
//!
//! This crate is `no_std` + `alloc`. It owns no audio apply: the bands design through
//! [`soul_audio_eq`] and apply through `soul_audio_core::Equalizer` — those crates are
//! already shared. The optional `serde` feature adds the [`format`] module, the
//! portable exchange format a baked profile is written to and the DAP reads back.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

// Re-export the band model + design surface so consumers get the whole EQ vocabulary
// through this crate (the desktop host used to re-export it from its own `eq` module).
pub use soul_audio_eq::{
    Band, BandKind, FREQ_MAX, FREQ_MIN, GAIN_LIMIT_DB, MAX_BANDS, Q_MAX, Q_MIN, band_magnitude_db,
    default_bands, design, eq_magnitude_db, format_autoeq, parse_autoeq,
};

#[cfg(feature = "serde")]
pub mod format;

/// One EQ unit in the DSP chain: its parametric bands, a preamp (headroom for boosts)
/// and an enable flag. The chain can hold several, applied in order.
#[derive(Debug, Clone, PartialEq)]
pub struct EqUnit {
    /// The parametric bands (0..=[`MAX_BANDS`]), applied as a series cascade.
    pub bands: Vec<Band>,
    /// A pre-gain in dB applied before this unit's bands (negative to leave headroom
    /// so boosts don't clip).
    pub preamp_db: f32,
    /// Whether this unit is engaged. A disabled unit is a bit-exact passthrough.
    pub enabled: bool,
    /// An optional user-given name for this unit (shown as "<name> (EQ)" in the UI,
    /// persisted so it travels with an audio profile / DAP export). `None` means the
    /// unit is unnamed and shows just its type ("EQ").
    pub name: Option<String>,
}

impl Default for EqUnit {
    fn default() -> Self {
        Self {
            bands: default_bands().to_vec(),
            preamp_db: 0.0,
            enabled: true,
            name: None,
        }
    }
}

impl EqUnit {
    /// Whether this unit, as configured, would change the signal (engaged, with a
    /// non-trivial preamp or some engaged band).
    #[must_use]
    pub fn affects(&self) -> bool {
        self.enabled && (self.preamp_db.abs() > 1e-3 || self.bands.iter().any(Band::affects))
    }
}

/// The type tag of a [`DspUnit`] — what kind of processor a chain entry is. The UI
/// type-picker and the serialization key off this; EQ is the only kind today, but the
/// chain is built to grow (compressor / limiter / … are future variants).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DspKind {
    /// A parametric-EQ unit ([`EqUnit`]).
    Eq,
}

/// One **typed** unit in the DSP chain. EQ is the only variant for now; the enum
/// exists so the chain generalizes beyond EQ without reshaping the apply engine — the
/// applier matches the variant and an unknown kind is a no-op passthrough.
#[derive(Debug, Clone, PartialEq)]
pub enum DspUnit {
    /// A parametric-EQ unit.
    Eq(EqUnit),
}

impl DspUnit {
    /// Convenience constructor for an EQ unit.
    #[must_use]
    pub fn eq(unit: EqUnit) -> Self {
        DspUnit::Eq(unit)
    }

    /// This unit's [`DspKind`] tag.
    #[must_use]
    pub fn kind(&self) -> DspKind {
        match self {
            DspUnit::Eq(_) => DspKind::Eq,
        }
    }

    /// Whether this unit is engaged. A disabled unit is a bit-exact passthrough.
    #[must_use]
    pub fn enabled(&self) -> bool {
        match self {
            DspUnit::Eq(eq) => eq.enabled,
        }
    }

    /// Enable or disable this unit (per-unit bypass, independent of the stage).
    pub fn set_enabled(&mut self, on: bool) {
        match self {
            DspUnit::Eq(eq) => eq.enabled = on,
        }
    }

    /// This unit's user-given name, if any (shown as "<name> (EQ)" in the UI). `None`
    /// for an unnamed unit.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match self {
            DspUnit::Eq(eq) => eq.name.as_deref(),
        }
    }

    /// Set (or clear, with `None`) this unit's user-given name.
    pub fn set_name(&mut self, name: Option<String>) {
        match self {
            DspUnit::Eq(eq) => eq.name = name,
        }
    }

    /// The inner [`EqUnit`] if this is an EQ unit, else `None`.
    #[must_use]
    pub fn as_eq(&self) -> Option<&EqUnit> {
        match self {
            DspUnit::Eq(eq) => Some(eq),
        }
    }

    /// The inner [`EqUnit`] mutably if this is an EQ unit, else `None`.
    #[must_use]
    pub fn as_eq_mut(&mut self) -> Option<&mut EqUnit> {
        match self {
            DspUnit::Eq(eq) => Some(eq),
        }
    }

    /// Whether this unit, as configured, would change the signal — delegates to the
    /// variant's own `affects` (an unknown future kind that does nothing is `false`).
    #[must_use]
    pub fn affects(&self) -> bool {
        match self {
            DspUnit::Eq(eq) => eq.affects(),
        }
    }
}

/// The DSP chain: an ordered list of typed DSP units (the signal flows through them in
/// order) plus a `generation` counter the applying sink watches to know when to
/// redesign its filters.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DspChain {
    /// The DSP units, applied front-to-back. Reordering changes processing order.
    pub units: Vec<DspUnit>,
    /// Bumped on every change so the applier knows to redesign.
    pub generation: u64,
}

impl DspChain {
    /// A chain of the given units, generation 0.
    #[must_use]
    pub fn new(units: Vec<DspUnit>) -> Self {
        Self {
            units,
            generation: 0,
        }
    }

    /// Whether any unit would change the signal. When `false` the applier passes the
    /// frames through untouched (bit-exact).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.units.iter().any(DspUnit::affects)
    }

    /// Bump the generation so a live applier redesigns on its next block.
    pub fn bump(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }
}

/// A named snapshot of a whole [`DspChain`] — an "audio profile".
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    /// The user-visible profile name.
    pub name: String,
    /// The chain this profile applies when active.
    pub chain: DspChain,
}

impl Profile {
    /// A profile with the given name and chain.
    #[must_use]
    pub fn new(name: impl Into<String>, chain: DspChain) -> Self {
        Self {
            name: name.into(),
            chain,
        }
    }
}

/// An ordered set of [`Profile`]s with exactly one active, plus the **pure** switching
/// / management logic shared by every head. Holds no I/O and no engine handle: each
/// head wraps a mutation with its own persist + engine-push. Kept non-empty by
/// construction — [`remove_active`](Self::remove_active) refuses to drop the last one.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileSet {
    profiles: Vec<Profile>,
    active: usize,
}

impl ProfileSet {
    /// A set from the given profiles and active index (clamped into range). The caller
    /// should pass at least one profile; an empty set has no active profile.
    #[must_use]
    pub fn new(profiles: Vec<Profile>, active: usize) -> Self {
        let active = if profiles.is_empty() {
            0
        } else {
            active.min(profiles.len() - 1)
        };
        Self { profiles, active }
    }

    /// A single-profile set (the common migrated-from-one-chain case).
    #[must_use]
    pub fn single(profile: Profile) -> Self {
        Self {
            profiles: alloc::vec![profile],
            active: 0,
        }
    }

    /// All profiles, in order.
    #[must_use]
    pub fn profiles(&self) -> &[Profile] {
        &self.profiles
    }

    /// How many profiles the set holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.profiles.len()
    }

    /// Whether the set is empty (true only for a degenerate empty construction).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }

    /// The active profile's index.
    #[must_use]
    pub fn active_index(&self) -> usize {
        self.active
    }

    /// The active profile, or `None` if the set is empty.
    #[must_use]
    pub fn active(&self) -> Option<&Profile> {
        self.profiles.get(self.active)
    }

    /// The active profile's chain, or `None` if the set is empty.
    #[must_use]
    pub fn active_chain(&self) -> Option<&DspChain> {
        self.active().map(|p| &p.chain)
    }

    /// Make profile `i` active. Returns whether the active profile changed (no-op when
    /// `i` is already active or out of range).
    pub fn switch(&mut self, i: usize) -> bool {
        if i >= self.profiles.len() || i == self.active {
            return false;
        }
        self.active = i;
        true
    }

    /// Replace the active profile's chain (the auto-save write-through: the live chain
    /// *is* the active profile). No-op on an empty set.
    pub fn set_active_chain(&mut self, chain: DspChain) {
        if let Some(p) = self.profiles.get_mut(self.active) {
            p.chain = chain;
        }
    }

    /// Append a profile with a unique-ified `name` and the given chain, make it active,
    /// and return its index.
    pub fn add_and_select(&mut self, name: &str, chain: DspChain) -> usize {
        let name = self.unique_name(name, None);
        self.profiles.push(Profile::new(name, chain));
        self.active = self.profiles.len() - 1;
        self.active
    }

    /// Append an empty-chain profile with a unique-ified `name`, make it active, and
    /// return its index.
    pub fn add_empty(&mut self, name: &str) -> usize {
        self.add_and_select(name, DspChain::default())
    }

    /// Duplicate the active profile (its chain) under a unique-ified `name`, make the
    /// copy active, and return its index. No-op (returns the current active) on an
    /// empty set.
    pub fn duplicate_active(&mut self, name: &str) -> usize {
        let Some(chain) = self.active_chain().cloned() else {
            return self.active;
        };
        self.add_and_select(name, chain)
    }

    /// Remove the active profile and switch to a neighbour. Refuses to remove the last
    /// profile (a set is never empty once non-empty). Returns whether one was removed.
    pub fn remove_active(&mut self) -> bool {
        if self.profiles.len() <= 1 {
            return false;
        }
        self.profiles.remove(self.active);
        self.active = self.active.min(self.profiles.len() - 1);
        true
    }

    /// Rename profile `i` (disambiguated against the others). Returns whether `i` was
    /// in range.
    pub fn rename(&mut self, i: usize, name: &str) -> bool {
        if i >= self.profiles.len() {
            return false;
        }
        let unique = self.unique_name(name, Some(i));
        self.profiles[i].name = unique;
        true
    }

    /// A name that doesn't collide with the others: `base`, else `base 2`, `base 3`, …
    /// The profile at `ignore` (if any) is skipped, so renaming a profile to its own
    /// current name doesn't append a number.
    #[must_use]
    pub fn unique_name(&self, base: &str, ignore: Option<usize>) -> String {
        let taken = |name: &str| {
            self.profiles
                .iter()
                .enumerate()
                .any(|(i, p)| Some(i) != ignore && p.name == name)
        };
        if !taken(base) {
            return base.to_string();
        }
        (2u32..)
            .map(|n| format!("{base} {n}"))
            .find(|cand| !taken(cand))
            .unwrap_or_else(|| base.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eq(preamp: f32, name: Option<&str>) -> DspUnit {
        DspUnit::Eq(EqUnit {
            preamp_db: preamp,
            name: name.map(String::from),
            ..EqUnit::default()
        })
    }

    #[test]
    fn chain_is_active_only_when_a_unit_affects() {
        let flat = EqUnit {
            enabled: false,
            ..EqUnit::default()
        };
        let chain = DspChain::new(alloc::vec![DspUnit::Eq(flat)]);
        assert!(!chain.is_active(), "a disabled unit is passthrough");
        let shaped = DspChain::new(alloc::vec![eq(6.0, None)]);
        assert!(shaped.is_active(), "a preamp makes the chain active");
    }

    #[test]
    fn profile_set_switches_and_writes_through() {
        let mut set = ProfileSet::new(
            alloc::vec![
                Profile::new("Flat", DspChain::new(alloc::vec![eq(0.0, None)])),
                Profile::new("Loud", DspChain::new(alloc::vec![eq(9.0, None)])),
            ],
            0,
        );
        assert_eq!(set.active().unwrap().name, "Flat");
        assert!(set.switch(1));
        assert_eq!(
            set.active_chain().unwrap().units[0]
                .as_eq()
                .unwrap()
                .preamp_db,
            9.0
        );
        assert!(!set.switch(1), "switching to the active index is a no-op");

        // Write-through replaces the active profile's chain.
        set.set_active_chain(DspChain::new(alloc::vec![eq(-3.0, None)]));
        assert_eq!(
            set.profiles()[1].chain.units[0].as_eq().unwrap().preamp_db,
            -3.0
        );
        assert_eq!(
            set.profiles()[0].chain.units[0].as_eq().unwrap().preamp_db,
            0.0,
            "the other profile is untouched"
        );
    }

    #[test]
    fn add_duplicate_delete_and_unique_names() {
        let mut set = ProfileSet::single(Profile::new(
            "Default",
            DspChain::new(alloc::vec![eq(-5.0, None)]),
        ));
        // New empty profile becomes active.
        let i = set.add_empty("New profile");
        assert_eq!(i, 1);
        assert!(set.active_chain().unwrap().units.is_empty());

        // Duplicate the active (empty) profile → unique "copy" name.
        set.switch(0);
        let d = set.duplicate_active("Default copy");
        assert_eq!(set.profiles()[d].name, "Default copy");
        assert_eq!(
            set.active_chain().unwrap().units[0]
                .as_eq()
                .unwrap()
                .preamp_db,
            -5.0
        );

        // Name collisions disambiguate.
        let again = set.duplicate_active("Default copy");
        assert_eq!(set.profiles()[again].name, "Default copy 2");

        // Rename to its own name is a no-op (not "name 2").
        assert!(set.rename(again, "Default copy 2"));
        assert_eq!(set.profiles()[again].name, "Default copy 2");

        // Delete down to the last; the last can't be removed.
        while set.len() > 1 {
            assert!(set.remove_active());
        }
        assert!(!set.remove_active(), "the last profile can't be removed");
        assert_eq!(set.len(), 1);
    }
}
