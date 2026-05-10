//! 16-point compass bearings + distance helpers for navigating between two
//! Guild Wars 2 map coordinates.
//!
//! ## Coordinate convention
//!
//! GW2 map coordinates use a **Y-down** convention: the Y axis grows
//! southwards (the same way screen pixels grow downward). This is the
//! convention reported by the Mumble Link `context` block's
//! `player_x` / `player_y`, and the same one used by `/v2/maps` for
//! `continent_rect` corner coordinates.
//!
//! All public functions in this module assume that convention. The
//! [`bearing`] helper flips the Y delta internally so the resulting
//! compass direction matches the *physical* world the player sees: north
//! is "up the map" (smaller Y), south is "down the map" (larger Y).
//!
//! ## Units
//!
//! GW2 spatial units are *inches* in the engine; one inch ≈ `0.0254` m.
//! That conversion is what [`distance_meters`] applies on top of
//! [`distance_units`].

use std::fmt;

/// Conversion factor from GW2 spatial units (≈ inches) to metres.
///
/// The Mumble Link spec puts `f_avatar_position` in metres directly, but
/// the *map* coordinates we care about for navigation are still in
/// engine units. This constant lets us quote distances to the player in
/// whichever unit the question came in.
pub const METERS_PER_GW2_UNIT: f64 = 0.0254;

/// 16-point compass direction. Variants are listed clockwise starting
/// from north so iteration order is the natural one for "tour the map".
///
/// `Same` is a sentinel used by [`bearing`] when the two coordinates
/// are within `f64::EPSILON` of each other. We surface it explicitly
/// rather than picking an arbitrary direction so callers can short-
/// circuit ("you are already there").
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
pub enum Bearing16 {
    N,
    NNE,
    NE,
    ENE,
    E,
    ESE,
    SE,
    SSE,
    S,
    SSW,
    SW,
    WSW,
    W,
    WNW,
    NW,
    NNW,
    /// `from` and `to` were the same point.
    Same,
}

impl Bearing16 {
    /// Stable short-form label. Matches the variant name for the 16
    /// compass points; `Same` renders as `"="` so it's distinguishable
    /// at a glance from the directional ones.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::N => "N",
            Self::NNE => "NNE",
            Self::NE => "NE",
            Self::ENE => "ENE",
            Self::E => "E",
            Self::ESE => "ESE",
            Self::SE => "SE",
            Self::SSE => "SSE",
            Self::S => "S",
            Self::SSW => "SSW",
            Self::SW => "SW",
            Self::WSW => "WSW",
            Self::W => "W",
            Self::WNW => "WNW",
            Self::NW => "NW",
            Self::NNW => "NNW",
            Self::Same => "=",
        }
    }

    /// Long-form direction name suitable for a sentence like
    /// "head <name> for ~120 m". `Same` returns "in place".
    #[must_use]
    pub fn long_name(self) -> &'static str {
        match self {
            Self::N => "north",
            Self::NNE => "north-northeast",
            Self::NE => "northeast",
            Self::ENE => "east-northeast",
            Self::E => "east",
            Self::ESE => "east-southeast",
            Self::SE => "southeast",
            Self::SSE => "south-southeast",
            Self::S => "south",
            Self::SSW => "south-southwest",
            Self::SW => "southwest",
            Self::WSW => "west-southwest",
            Self::W => "west",
            Self::WNW => "west-northwest",
            Self::NW => "northwest",
            Self::NNW => "north-northwest",
            Self::Same => "in place",
        }
    }
}

impl fmt::Display for Bearing16 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Convert a bearing in degrees clockwise from north (0 ≤ θ < 360) into
/// the closest of the 16 compass points.
///
/// Each compass point covers a 22.5° wedge centred on its cardinal angle.
fn from_degrees_cw(deg: f64) -> Bearing16 {
    // Normalise to [0, 360). `deg.rem_euclid(360.0)` handles negatives
    // and very large positives in one shot.
    let theta = deg.rem_euclid(360.0);
    // 16 wedges of 22.5° each, centred on N=0, NNE=22.5, NE=45, ...
    // The wedge index is `floor((theta + 11.25) / 22.5) mod 16`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let idx = ((theta + 11.25) / 22.5).floor() as i64 % 16;
    // `idx` is in [0, 15] after the mod; cast is safe even on 32-bit.
    let i = usize::try_from(idx.rem_euclid(16)).unwrap_or(0);
    [
        Bearing16::N,
        Bearing16::NNE,
        Bearing16::NE,
        Bearing16::ENE,
        Bearing16::E,
        Bearing16::ESE,
        Bearing16::SE,
        Bearing16::SSE,
        Bearing16::S,
        Bearing16::SSW,
        Bearing16::SW,
        Bearing16::WSW,
        Bearing16::W,
        Bearing16::WNW,
        Bearing16::NW,
        Bearing16::NNW,
    ][i]
}

/// Compute the 16-point compass bearing from `from` to `to`, both in GW2
/// map coordinates (Y-down, see module docs).
///
/// The angle is `atan2(dx, -dy)` (note the inverted dy) so that a
/// positive Y delta in map coords renders as *south*, not north.
#[must_use]
pub fn bearing(from: (f64, f64), to: (f64, f64)) -> Bearing16 {
    let (x1, y1) = from;
    let (x2, y2) = to;
    let dx = x2 - x1;
    // Y is inverted: increasing Y on the map = south. Negate so positive
    // y-delta becomes "down" (south) in atan2-clockwise-from-north space.
    let dy_geo = -(y2 - y1);

    if dx.abs() < f64::EPSILON && dy_geo.abs() < f64::EPSILON {
        return Bearing16::Same;
    }

    // atan2(dx, dy_geo) gives an angle in radians clockwise from north
    // (positive Y is north, positive X is east — the standard "compass"
    // setup we set up via the dy negation above).
    let radians = dx.atan2(dy_geo);
    let degrees = radians.to_degrees();
    from_degrees_cw(degrees)
}

/// Euclidean distance between two coordinates in raw GW2 units.
#[must_use]
pub fn distance_units(from: (f64, f64), to: (f64, f64)) -> f64 {
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    dx.hypot(dy)
}

/// Same as [`distance_units`] but converted to metres via
/// [`METERS_PER_GW2_UNIT`].
#[must_use]
pub fn distance_meters(from: (f64, f64), to: (f64, f64)) -> f64 {
    distance_units(from, to) * METERS_PER_GW2_UNIT
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1 unit "above" (negative Y) is north because Y is flipped.
    #[test]
    fn unit_north_is_n() {
        assert_eq!(bearing((0.0, 0.0), (0.0, -1.0)), Bearing16::N);
    }

    #[test]
    fn unit_south_is_s() {
        assert_eq!(bearing((0.0, 0.0), (0.0, 1.0)), Bearing16::S);
    }

    #[test]
    fn unit_east_is_e() {
        assert_eq!(bearing((0.0, 0.0), (1.0, 0.0)), Bearing16::E);
    }

    #[test]
    fn unit_west_is_w() {
        assert_eq!(bearing((0.0, 0.0), (-1.0, 0.0)), Bearing16::W);
    }

    #[test]
    fn diagonal_ne_is_ne() {
        assert_eq!(bearing((0.0, 0.0), (1.0, -1.0)), Bearing16::NE);
    }

    #[test]
    fn diagonal_se_is_se() {
        assert_eq!(bearing((0.0, 0.0), (1.0, 1.0)), Bearing16::SE);
    }

    #[test]
    fn diagonal_sw_is_sw() {
        assert_eq!(bearing((0.0, 0.0), (-1.0, 1.0)), Bearing16::SW);
    }

    #[test]
    fn diagonal_nw_is_nw() {
        assert_eq!(bearing((0.0, 0.0), (-1.0, -1.0)), Bearing16::NW);
    }

    /// Sub-cardinals: 22.5° increments. Pick a vector that lands cleanly
    /// inside each wedge. Use `(sin θ, -cos θ)` so the angle is θ
    /// clockwise from north in the Y-up frame, then flip Y to apply the
    /// map convention.
    #[test]
    fn sub_cardinals_round_to_correct_wedge() {
        let cases: &[(f64, Bearing16)] = &[
            (0.0, Bearing16::N),
            (22.5, Bearing16::NNE),
            (45.0, Bearing16::NE),
            (67.5, Bearing16::ENE),
            (90.0, Bearing16::E),
            (112.5, Bearing16::ESE),
            (135.0, Bearing16::SE),
            (157.5, Bearing16::SSE),
            (180.0, Bearing16::S),
            (202.5, Bearing16::SSW),
            (225.0, Bearing16::SW),
            (247.5, Bearing16::WSW),
            (270.0, Bearing16::W),
            (292.5, Bearing16::WNW),
            (315.0, Bearing16::NW),
            (337.5, Bearing16::NNW),
        ];
        for (deg, want) in cases {
            let theta = deg.to_radians();
            // Y-up unit vector in the geographic frame.
            let dx = theta.sin();
            let dy_geo = theta.cos();
            // Flip back to Y-down map space for the test input.
            let to = (dx, -dy_geo);
            assert_eq!(
                bearing((0.0, 0.0), to),
                *want,
                "expected {want:?} at {deg}°"
            );
        }
    }

    /// Wedge boundaries should fall consistently. Just inside each wedge
    /// we should still get the wedge's direction; nudges past the
    /// boundary tip to the next one.
    #[test]
    fn wedge_boundaries_are_stable() {
        // 11.24° (just under N/NNE boundary) → N
        let theta = 11.24_f64.to_radians();
        let to = (theta.sin(), -theta.cos());
        assert_eq!(bearing((0.0, 0.0), to), Bearing16::N);

        // 11.26° (just past) → NNE
        let theta = 11.26_f64.to_radians();
        let to = (theta.sin(), -theta.cos());
        assert_eq!(bearing((0.0, 0.0), to), Bearing16::NNE);
    }

    #[test]
    fn same_point_returns_same() {
        assert_eq!(bearing((100.0, 200.0), (100.0, 200.0)), Bearing16::Same);
    }

    #[test]
    fn distance_units_is_pythagorean() {
        let d = distance_units((0.0, 0.0), (3.0, 4.0));
        assert!((d - 5.0).abs() < 1e-9);
    }

    #[test]
    fn distance_meters_applies_inch_conversion() {
        let d = distance_meters((0.0, 0.0), (1.0, 0.0));
        assert!((d - METERS_PER_GW2_UNIT).abs() < 1e-12);
    }

    #[test]
    fn bearing_display_matches_label() {
        assert_eq!(format!("{}", Bearing16::NNE), "NNE");
        assert_eq!(format!("{}", Bearing16::Same), "=");
    }

    #[test]
    fn long_names_are_present_for_every_variant() {
        // Smoke check — make sure the enum and the match stay in sync.
        for b in [
            Bearing16::N,
            Bearing16::NNE,
            Bearing16::NE,
            Bearing16::ENE,
            Bearing16::E,
            Bearing16::ESE,
            Bearing16::SE,
            Bearing16::SSE,
            Bearing16::S,
            Bearing16::SSW,
            Bearing16::SW,
            Bearing16::WSW,
            Bearing16::W,
            Bearing16::WNW,
            Bearing16::NW,
            Bearing16::NNW,
            Bearing16::Same,
        ] {
            assert!(!b.long_name().is_empty(), "{b:?} missing long name");
        }
    }
}
