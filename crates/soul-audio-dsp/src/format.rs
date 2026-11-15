//! The portable **exchange format** for DSP chains and audio profiles — the serde
//! structs a baked profile is written to and the DAP reads back. A single Rust
//! definition both heads compile against, so an exported profile is readable by
//! construction. The *serializer* is the caller's choice (the desktop writes JSON via
//! `serde_json`; the DAP can use the same, or postcard); only these shapes are shared.
//!
//! Each unit carries a string `kind` tag and unknown kinds are skipped on load, so a
//! file from a newer build (a future compressor / limiter unit) still loads what this
//! build understands. The band `kind` is likewise a stable string token.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::{Band, BandKind, DspChain, DspUnit, EqUnit, MAX_BANDS, Profile, ProfileSet};

/// The stable on-disk tag for an EQ unit's `kind`. A file written before the chain grew
/// beyond EQ has no `kind` field, so it defaults to this (= still EQ).
fn default_dsp_kind() -> String {
    "eq".to_string()
}

/// One persisted parametric band — a TOML/JSON-friendly mirror of [`Band`] (the `kind`
/// is a stable string token; `gain` maps to `Band::gain_db`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BandCfg {
    pub kind: String,
    pub freq: f32,
    pub gain: f32,
    pub q: f32,
    pub enabled: bool,
}

impl From<&Band> for BandCfg {
    fn from(b: &Band) -> Self {
        Self {
            kind: kind_as_str(b.kind).to_string(),
            freq: b.freq,
            gain: b.gain_db,
            q: b.q,
            enabled: b.enabled,
        }
    }
}

impl BandCfg {
    /// Build a [`Band`], falling back to `default` for an unknown kind token.
    #[must_use]
    pub fn to_band(&self, default: &Band) -> Band {
        Band {
            kind: kind_from_str(&self.kind).unwrap_or(default.kind),
            freq: self.freq,
            gain_db: self.gain,
            q: self.q,
            enabled: self.enabled,
        }
    }
}

fn kind_as_str(kind: BandKind) -> &'static str {
    match kind {
        BandKind::LowShelf => "low_shelf",
        BandKind::Peaking => "peaking",
        BandKind::HighShelf => "high_shelf",
        BandKind::LowCut => "low_cut",
        BandKind::HighCut => "high_cut",
        BandKind::Notch => "notch",
    }
}

fn kind_from_str(s: &str) -> Option<BandKind> {
    Some(match s.trim() {
        "low_shelf" => BandKind::LowShelf,
        "peaking" => BandKind::Peaking,
        "high_shelf" => BandKind::HighShelf,
        "low_cut" => BandKind::LowCut,
        "high_cut" => BandKind::HighCut,
        "notch" => BandKind::Notch,
        _ => return None,
    })
}

/// One persisted DSP-chain unit: a `kind` tag plus the EQ fields. An unknown `kind`
/// round-trips on disk but is skipped when building the live chain (forward-compat).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DspUnitCfg {
    #[serde(default = "default_dsp_kind")]
    pub kind: String,
    /// The user-given unit name, if any (skipped when `None`, defaulted so old files
    /// without the field load as unnamed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub enabled: bool,
    pub preamp: f32,
    pub bands: Vec<BandCfg>,
}

impl From<&DspUnit> for DspUnitCfg {
    fn from(u: &DspUnit) -> Self {
        match u {
            DspUnit::Eq(eq) => Self {
                kind: "eq".to_string(),
                name: u.name().map(String::from),
                enabled: eq.enabled,
                preamp: eq.preamp_db,
                bands: eq.bands.iter().map(BandCfg::from).collect(),
            },
        }
    }
}

impl DspUnitCfg {
    /// Build a live [`DspUnit`], or `None` for an unknown `kind`.
    #[must_use]
    pub fn to_unit(&self) -> Option<DspUnit> {
        match self.kind.trim() {
            "eq" => {
                let fallback = Band {
                    kind: BandKind::Peaking,
                    freq: 1000.0,
                    gain_db: 0.0,
                    q: 1.0,
                    enabled: true,
                };
                Some(DspUnit::Eq(EqUnit {
                    bands: self
                        .bands
                        .iter()
                        .take(MAX_BANDS)
                        .map(|b| b.to_band(&fallback))
                        .collect(),
                    preamp_db: self.preamp,
                    enabled: self.enabled,
                    name: self.name.clone(),
                }))
            }
            _ => None,
        }
    }
}

/// One persisted audio profile: a name plus the whole DSP chain. Also the shape of an
/// exported `*.json` profile file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileCfg {
    pub name: String,
    pub chain: Vec<DspUnitCfg>,
}

impl ProfileCfg {
    /// Decode to a live [`Profile`], skipping unknown-kind units.
    #[must_use]
    pub fn to_profile(&self) -> Profile {
        Profile::new(
            self.name.clone(),
            DspChain::new(self.chain.iter().filter_map(DspUnitCfg::to_unit).collect()),
        )
    }

    /// Encode a live [`Profile`] for persistence / export.
    #[must_use]
    pub fn from_profile(p: &Profile) -> Self {
        Self {
            name: p.name.clone(),
            chain: p.chain.units.iter().map(DspUnitCfg::from).collect(),
        }
    }
}

/// A persisted [`ProfileSet`] — the profiles plus the active index. The whole audio-
/// profile store, as a desktop config writes it and the DAP reads it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileSetCfg {
    pub profiles: Vec<ProfileCfg>,
    #[serde(default)]
    pub active: usize,
}

impl ProfileSetCfg {
    /// Decode to a live [`ProfileSet`] (active index clamped into range).
    #[must_use]
    pub fn to_set(&self) -> ProfileSet {
        ProfileSet::new(
            self.profiles.iter().map(ProfileCfg::to_profile).collect(),
            self.active,
        )
    }

    /// Encode a live [`ProfileSet`].
    #[must_use]
    pub fn from_set(set: &ProfileSet) -> Self {
        Self {
            profiles: set
                .profiles()
                .iter()
                .map(ProfileCfg::from_profile)
                .collect(),
            active: set.active_index(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn profile_round_trips_and_unknown_kind_is_skipped() {
        let profile = Profile::new(
            "Studio",
            DspChain::new(vec![DspUnit::Eq(EqUnit {
                preamp_db: -3.0,
                name: Some("low".to_string()),
                ..EqUnit::default()
            })]),
        );
        let cfg = ProfileCfg::from_profile(&profile);
        assert_eq!(
            cfg.to_profile(),
            profile,
            "a profile round-trips through the cfg"
        );

        // An unknown unit kind is dropped on load (forward-compat).
        let mut cfg2 = cfg.clone();
        cfg2.chain.push(DspUnitCfg {
            kind: "compressor".to_string(),
            name: None,
            enabled: true,
            preamp: 0.0,
            bands: Vec::new(),
        });
        assert_eq!(
            cfg2.to_profile().chain.units.len(),
            1,
            "the unknown kind is skipped, the EQ unit survives"
        );
    }

    #[test]
    fn untagged_unit_loads_as_eq_and_set_round_trips() {
        // A unit cfg with no `kind` (an old file) deserializes as EQ via the default.
        let unit: DspUnitCfg =
            serde_json::from_str(r#"{"enabled":true,"preamp":1.0,"bands":[]}"#).unwrap();
        assert_eq!(unit.kind, "eq");
        assert!(unit.to_unit().unwrap().as_eq().is_some());

        let set = ProfileSet::new(
            vec![
                Profile::new("A", DspChain::new(vec![DspUnit::Eq(EqUnit::default())])),
                Profile::new("B", DspChain::default()),
            ],
            1,
        );
        let back = ProfileSetCfg::from_set(&set).to_set();
        assert_eq!(back.active_index(), 1);
        assert_eq!(back.profiles().len(), 2);
        assert_eq!(back.active().unwrap().name, "B");
    }
}
