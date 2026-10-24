//! Piecewise-linear fan duty curves.
//!
//! A [`CurveDef`] maps a temperature (°C) to a fan duty cycle (percent).
//! Evaluation is clamped at both ends: below the first point the first duty
//! applies, above the last point the last duty applies. A single-point curve is
//! a constant-duty curve.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Highest temperature a curve point may carry; anything above is a config
/// mistake (thermal shutdown territory, not a sane control target).
pub const MAX_CURVE_TEMP_C: f32 = 110.0;

/// One `[temp_c, duty_pct]` pair. Serializes as a two-element array so curves
/// read naturally in TOML: `points = [[45, 30], [60, 50]]`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CurvePoint(pub f32, pub u8);

/// Ways a curve's points can be invalid.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum CurveError {
    /// A curve with no points has no defined duty.
    #[error("curve needs at least one point")]
    Empty,
    /// Duty cycles are percentages.
    #[error("duty {duty} at {temp}°C is outside 0-100")]
    DutyOutOfRange {
        /// Temperature of the offending point.
        temp: f32,
        /// Duty that was out of range.
        duty: u8,
    },
    /// Reject nonsense temperatures (negative, NaN, above the sane ceiling).
    #[error("temperature {temp}°C is outside 0-{MAX_CURVE_TEMP_C}°C")]
    TempOutOfRange {
        /// Temperature that was out of range.
        temp: f32,
    },
    /// Strictly increasing temperatures are required for interpolation.
    #[error(
        "temperatures must be strictly increasing: point {index} ({cur}°C) must be above {prev}°C"
    )]
    NotIncreasing {
        /// Index of the offending point.
        index: usize,
        /// Previous point's temperature.
        prev: f32,
        /// Offending temperature.
        cur: f32,
    },
}

/// A validated set of points forming a piecewise-linear curve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CurveDef {
    /// Sorted, strictly increasing `[temp_c, duty_pct]` pairs.
    pub points: Vec<CurvePoint>,
}

impl CurveDef {
    /// Build a curve from points without validating; prefer
    /// [`CurveDef::validated`] when the data is untrusted.
    pub fn from_points(points: Vec<CurvePoint>) -> Self {
        Self { points }
    }

    /// Validate points and return the curve, or the first problem found.
    pub fn validated(points: Vec<CurvePoint>) -> Result<Self, CurveError> {
        let curve = Self { points };
        curve.validate()?;
        Ok(curve)
    }

    /// Check every invariant: non-empty, sane temps, duties in 0-100,
    /// strictly increasing temps.
    pub fn validate(&self) -> Result<(), CurveError> {
        let mut prev: Option<f32> = None;
        for (index, CurvePoint(temp, duty)) in self.points.iter().enumerate() {
            if !temp.is_finite() || *temp < 0.0 || *temp > MAX_CURVE_TEMP_C {
                return Err(CurveError::TempOutOfRange { temp: *temp });
            }
            if *duty > 100 {
                return Err(CurveError::DutyOutOfRange {
                    temp: *temp,
                    duty: *duty,
                });
            }
            if let Some(prev) = prev
                && *temp <= prev
            {
                return Err(CurveError::NotIncreasing {
                    index,
                    prev,
                    cur: *temp,
                });
            }
            prev = Some(*temp);
        }
        if self.points.is_empty() {
            return Err(CurveError::Empty);
        }
        Ok(())
    }

    /// Duty (percent) at `temp_c`, piecewise-linear with clamping at both
    /// ends. Assumes the curve was validated (monotonic temps); invalid
    /// curves still return *something* rather than panicking — the first
    /// point at or above `temp_c` bounds the result.
    pub fn duty_at(&self, temp_c: f32) -> u8 {
        let Some(first) = self.points.first() else {
            return 0;
        };
        if temp_c <= first.0 {
            return first.1;
        }
        let Some(last) = self.points.last() else {
            return 0;
        };
        if temp_c >= last.0 {
            return last.1;
        }
        for pair in self.points.windows(2) {
            let (t0, d0) = (pair[0].0, pair[0].1);
            let (t1, d1) = (pair[1].0, pair[1].1);
            if temp_c < t1 {
                if t1 <= t0 {
                    return d1.max(d0);
                }
                let frac = (temp_c - t0) / (t1 - t0);
                let duty = f32::from(d0) + frac * (f32::from(d1) - f32::from(d0));
                return duty.round().clamp(0.0, 100.0) as u8;
            }
        }
        last.1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curve(points: &[(f32, u8)]) -> CurveDef {
        CurveDef::from_points(points.iter().map(|&(t, d)| CurvePoint(t, d)).collect())
    }

    #[test]
    fn interpolates_between_points() {
        let c = curve(&[(40.0, 20), (60.0, 40)]);
        assert_eq!(c.duty_at(20.0), 20); // clamped below
        assert_eq!(c.duty_at(40.0), 20);
        assert_eq!(c.duty_at(50.0), 30);
        assert_eq!(c.duty_at(55.0), 35);
        assert_eq!(c.duty_at(60.0), 40);
        assert_eq!(c.duty_at(99.0), 40); // clamped above
    }

    #[test]
    fn rounds_to_nearest_duty() {
        let c = curve(&[(0.0, 0), (10.0, 10)]);
        assert_eq!(c.duty_at(5.4), 5);
        assert_eq!(c.duty_at(5.5), 6); // .5 rounds up
        assert_eq!(c.duty_at(5.6), 6);
    }

    #[test]
    fn single_point_is_constant() {
        let c = curve(&[(70.0, 55)]);
        assert_eq!(c.duty_at(0.0), 55);
        assert_eq!(c.duty_at(70.0), 55);
        assert_eq!(c.duty_at(110.0), 55);
    }

    #[test]
    fn validation_rejects_bad_input() {
        assert_eq!(CurveDef::validated(vec![]).unwrap_err(), CurveError::Empty);
        assert_eq!(
            curve(&[(50.0, 101)]).validate().unwrap_err(),
            CurveError::DutyOutOfRange {
                temp: 50.0,
                duty: 101
            }
        );
        assert_eq!(
            curve(&[(-1.0, 50)]).validate().unwrap_err(),
            CurveError::TempOutOfRange { temp: -1.0 }
        );
        assert!(matches!(
            curve(&[(f32::NAN, 50)]).validate().unwrap_err(),
            CurveError::TempOutOfRange { .. }
        ));
        assert_eq!(
            curve(&[(50.0, 10), (50.0, 20)]).validate().unwrap_err(),
            CurveError::NotIncreasing {
                index: 1,
                prev: 50.0,
                cur: 50.0
            }
        );
        assert_eq!(
            curve(&[(60.0, 10), (50.0, 20)]).validate().unwrap_err(),
            CurveError::NotIncreasing {
                index: 1,
                prev: 60.0,
                cur: 50.0
            }
        );
    }

    #[test]
    fn toml_roundtrip() {
        #[derive(serde::Deserialize, serde::Serialize)]
        struct Wrap {
            points: Vec<CurvePoint>,
        }
        let w = Wrap {
            points: vec![CurvePoint(45.0, 30), CurvePoint(85.5, 100)],
        };
        let s = toml::to_string(&w).unwrap();
        assert!(s.contains("45.0, 30"), "inline array form: {s}");
        let back: Wrap = toml::from_str(&s).unwrap();
        assert_eq!(back.points, w.points);
    }
}
