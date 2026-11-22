//! The D-Bus **audio device reservation** protocol (`org.freedesktop.ReserveDevice1`)
//! — how a Linux application takes a sound card away from the running audio server.
//!
//! Opening `hw:CARD,DEV` directly is what makes playback bit-perfect, but on a normal
//! desktop PipeWire (via WirePlumber) or PulseAudio is already managing that card. They
//! don't hold the ALSA device open permanently, so a direct open often *appears* to
//! work — right up until the server grabs the card back for a notification sound and the
//! two fight over it. The reservation protocol is the agreed way to avoid that: a client
//! claims a well-known bus name for the card, and every mainstream Linux audio server
//! watches for it and releases the hardware.
//!
//! The name is `org.freedesktop.ReserveDevice1.Audio<N>` for ALSA card `N`. Claiming it
//! with `ALLOW_REPLACEMENT | REPLACE_EXISTING | DO_NOT_QUEUE` asks the current holder
//! (WirePlumber parks every card at priority −20) to hand the device over, and refuses
//! rather than queueing if it won't. Dropping the connection releases the name, so the
//! server takes the card back — including if this process dies, since the bus notices
//! the peer disappearing.
//!
//! Deliberately a *polite* claim: this never steals a device from another application
//! that holds it for its own exclusive playback. That case surfaces as
//! [`ReserveError::InUse`] so the caller can fall back to shared output rather than
//! interrupting whatever is already playing.

use zbus::blocking::Connection;
use zbus::blocking::fdo::DBusProxy;
use zbus::names::WellKnownName;

/// Why a device could not be reserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReserveError {
    /// Another client holds the device and would not release it.
    InUse,
    /// No session bus (a headless or service context), or the bus rejected the request.
    /// Not fatal on its own — a machine with no audio server has nothing to negotiate
    /// with, and the caller may still open the card directly.
    NoBus(String),
}

impl core::fmt::Display for ReserveError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InUse => write!(f, "another application holds the device"),
            Self::NoBus(e) => write!(f, "device reservation unavailable: {e}"),
        }
    }
}

impl std::error::Error for ReserveError {}

/// The reservation bus name for ALSA card `card`, per the protocol's convention.
///
/// Split out and tested because getting it wrong fails *silently* in the worst way:
/// the claim succeeds (nobody owns the misspelled name), the audio server never hears
/// about it, and playback is left fighting the server for the card.
#[must_use]
pub fn bus_name_for_card(card: i32) -> String {
    format!("org.freedesktop.ReserveDevice1.Audio{card}")
}

/// A held reservation. Dropping it releases the name, which hands the card back to
/// whichever audio server was managing it.
#[derive(Debug)]
pub struct Reservation {
    /// Kept alive because the name is owned for exactly as long as this connection is:
    /// closing it is what releases the device.
    _conn: Connection,
    card: i32,
}

impl Reservation {
    /// The ALSA card this reservation covers.
    #[must_use]
    pub fn card(&self) -> i32 {
        self.card
    }

    /// Ask the session bus for ALSA card `card`, asking the current holder to release it.
    ///
    /// Returns [`ReserveError::InUse`] when the holder refuses — never a forced takeover.
    pub fn acquire(card: i32) -> Result<Self, ReserveError> {
        let conn = Connection::session().map_err(|e| ReserveError::NoBus(e.to_string()))?;
        let proxy = DBusProxy::new(&conn).map_err(|e| ReserveError::NoBus(e.to_string()))?;
        let name = bus_name_for_card(card);
        let well_known = WellKnownName::try_from(name.as_str())
            .map_err(|e| ReserveError::NoBus(format!("bad reservation name {name}: {e}")))?;

        use zbus::fdo::{RequestNameFlags, RequestNameReply};
        let flags = RequestNameFlags::AllowReplacement
            | RequestNameFlags::ReplaceExisting
            | RequestNameFlags::DoNotQueue;
        match proxy.request_name(well_known, flags) {
            // `AlreadyOwner` can happen if this process reserved the card earlier and the
            // previous `Reservation` has not dropped yet; we own it either way.
            Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {
                Ok(Self { _conn: conn, card })
            }
            // The holder declined to be replaced, or we were queued behind it.
            Ok(RequestNameReply::Exists | RequestNameReply::InQueue) => Err(ReserveError::InUse),
            Err(e) => Err(ReserveError::NoBus(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The protocol's name convention, exactly. A typo here would claim a name no audio
    /// server is watching, so the reservation would "succeed" while PipeWire kept the
    /// card — the failure mode this test exists to prevent.
    #[test]
    fn bus_name_follows_the_reservation_convention() {
        assert_eq!(
            bus_name_for_card(0),
            "org.freedesktop.ReserveDevice1.Audio0"
        );
        assert_eq!(
            bus_name_for_card(3),
            "org.freedesktop.ReserveDevice1.Audio3"
        );
    }

    /// A live claim against the real session bus: after acquiring, the bus must report
    /// the name as OWNED, and dropping must release it.
    ///
    /// Worth a real-bus test because every failure here is silent. A claim that never
    /// reached the bus, or one the current holder quietly queued us behind, both look
    /// exactly like success from inside this process — and the only visible symptom
    /// would be the audio server taking the card back mid-track.
    ///
    /// `#[ignore]`d: it needs a session bus, and it briefly asks the desktop's audio
    /// server to release card 0.
    #[test]
    #[ignore = "claims a real sound card on the session bus; run with --ignored"]
    fn acquiring_takes_the_name_on_the_bus_and_dropping_releases_it() {
        let name = bus_name_for_card(0);
        let conn = Connection::session().expect("a session bus is required for this probe");
        let proxy = DBusProxy::new(&conn).expect("open the D-Bus proxy");
        let owned = || {
            proxy
                .list_names()
                .expect("list bus names")
                .iter()
                .any(|n| n.as_str() == name)
        };

        let reservation = match Reservation::acquire(0) {
            Ok(r) => r,
            Err(ReserveError::InUse) => {
                println!("card 0 held by another application — nothing to probe");
                return;
            }
            Err(e) => panic!("could not reserve card 0: {e}"),
        };
        assert_eq!(reservation.card(), 0);
        assert!(
            owned(),
            "the reservation claims to be held, but {name} is not owned on the bus — \
             the audio server would never have heard about it"
        );

        drop(reservation);
        assert!(
            !owned(),
            "dropping the reservation must release {name}, handing the card back"
        );
    }

    /// Every name the convention produces must be a legal D-Bus well-known name, or the
    /// request is rejected before it reaches the bus.
    #[test]
    fn generated_names_are_valid_dbus_well_known_names() {
        for card in [0, 1, 7, 31] {
            let name = bus_name_for_card(card);
            assert!(
                WellKnownName::try_from(name.as_str()).is_ok(),
                "{name} is not a valid well-known name"
            );
        }
    }
}
