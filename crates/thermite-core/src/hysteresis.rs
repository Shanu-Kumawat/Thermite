//! Hysteresis and temporal smoothing for curve-driven duty changes.
//!
//! Two independent mechanisms keep fans from oscillating:
//!
//! 1. **Spatial gap** (`drop_c`): after the fan steps *up* at temperature
//!    `t_up`, it may only step *down* once the temperature has fallen to
//!    `t_up - drop_c` or below. The gate tracks the most recent upshift.
//! 2. **Temporal hold** (`hold_ticks`): a decrease must persist for that many
//!    consecutive ticks before it is committed (tracking the lowest target
//!    seen in the window). Increases always commit immediately — spinning up
//!    is cheap, heat is not.

use crate::curve::CurveDef;

/// Smoothing state for one fan. Feed it a temperature every tick; it returns
/// the duty to command.
#[derive(Debug, Clone)]
pub struct Hysteresis {
    /// Temperature drop (°C) required after an upshift before a downshift is
    /// considered.
    pub drop_c: f32,
    /// Consecutive ticks a lower duty must hold before it is committed.
    pub hold_ticks: u32,
    last_output: Option<u8>,
    /// Temperature at the most recent committed *increase*; downshifts are
    /// blocked until the temperature falls `drop_c` below it.
    gate_temp: Option<f32>,
    pending: Option<(u8, u32)>, // (target duty, ticks seen so far)
}

impl Hysteresis {
    /// New smoother with the given gap and hold time.
    pub fn new(drop_c: f32, hold_ticks: u32) -> Self {
        Self {
            drop_c: drop_c.max(0.0),
            hold_ticks: hold_ticks.max(1),
            last_output: None,
            gate_temp: None,
            pending: None,
        }
    }

    /// Duty to command this tick for `temp_c` under `curve`.
    pub fn update(&mut self, temp_c: f32, curve: &CurveDef) -> u8 {
        let direct = curve.duty_at(temp_c);

        let Some(last) = self.last_output else {
            // First tick: no history, no smoothing.
            self.last_output = Some(direct);
            self.gate_temp = Some(temp_c);
            return direct;
        };

        let downshift_allowed = self
            .gate_temp
            .is_some_and(|gate| temp_c <= gate - self.drop_c);
        let candidate = if direct >= last || downshift_allowed {
            direct
        } else {
            last // spatial gate: hold the current duty until temp falls enough
        };

        if candidate >= last {
            // Equal-or-higher duty commits immediately; an equality after a
            // blocked decrease means the temperature came back — clear any
            // pending decrease.
            self.pending = None;
            if candidate > last {
                self.gate_temp = Some(temp_c);
            }
            self.last_output = Some(candidate);
            candidate
        } else {
            // Decrease: needs `hold_ticks` consecutive confirmations,
            // tracking the lowest target seen during the hold window.
            let (target, ticks) = self
                .pending
                .map_or((candidate, 1), |(t, n)| (t.min(candidate), n + 1));
            if ticks >= self.hold_ticks {
                self.pending = None;
                self.last_output = Some(target);
                target
            } else {
                self.pending = Some((target, ticks));
                last
            }
        }
    }

    /// Drop all smoothing state (used after resume or mode changes).
    pub fn reset(&mut self) {
        self.last_output = None;
        self.gate_temp = None;
        self.pending = None;
    }

    /// Duty most recently returned by [`Hysteresis::update`].
    pub fn last_output(&self) -> Option<u8> {
        self.last_output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curve::CurvePoint;

    fn curve() -> CurveDef {
        // duty_at: 50°C->30, 55->40, 60->50, 64->58, 65->60, 69->68, 70->70
        CurveDef::from_points(
            [(50.0, 30), (60.0, 50), (70.0, 70)]
                .iter()
                .map(|&(t, d)| CurvePoint(t, d))
                .collect(),
        )
    }

    #[test]
    fn first_tick_has_no_smoothing() {
        let mut h = Hysteresis::new(2.0, 5);
        assert_eq!(h.update(65.0, &curve()), 60);
    }

    #[test]
    fn increases_commit_immediately() {
        let mut h = Hysteresis::new(2.0, 5);
        assert_eq!(h.update(55.0, &curve()), 40);
        assert_eq!(h.update(65.0, &curve()), 60); // no hold on the way up
        assert_eq!(h.update(69.0, &curve()), 68);
    }

    #[test]
    fn decreases_need_hold_ticks() {
        let mut h = Hysteresis::new(0.0, 3);
        assert_eq!(h.update(70.0, &curve()), 70);
        assert_eq!(h.update(60.0, &curve()), 70); // tick 1: hold
        assert_eq!(h.update(60.0, &curve()), 70); // tick 2: hold
        assert_eq!(h.update(60.0, &curve()), 50); // tick 3: commit
    }

    #[test]
    fn spatial_gap_blocks_downshift_until_temp_falls() {
        // Upshift at 70°C; with drop_c = 5 a downshift needs temp <= 65°C.
        let mut h = Hysteresis::new(5.0, 1);
        assert_eq!(h.update(70.0, &curve()), 70);
        assert_eq!(h.update(66.0, &curve()), 70); // blocked: 66 > 65
        assert_eq!(h.update(60.0, &curve()), 50); // 60 <= 65: downshift commits (hold=1)
    }

    #[test]
    fn gate_tracks_the_latest_upshift() {
        let mut h = Hysteresis::new(5.0, 1);
        assert_eq!(h.update(70.0, &curve()), 70); // gate = 70
        assert_eq!(h.update(60.0, &curve()), 50); // allowed (60 <= 65), committed
        assert_eq!(h.update(65.0, &curve()), 60); // upshift: gate now 65
        assert_eq!(h.update(64.0, &curve()), 60); // blocked (64 > 60)
        assert_eq!(h.update(59.0, &curve()), 48); // allowed (59 <= 60)
    }

    #[test]
    fn hold_tracks_lowest_target_during_window() {
        let mut h = Hysteresis::new(0.0, 3);
        assert_eq!(h.update(70.0, &curve()), 70);
        assert_eq!(h.update(60.0, &curve()), 70);
        assert_eq!(h.update(50.0, &curve()), 70); // target fell further mid-hold
        assert_eq!(h.update(50.0, &curve()), 30); // commits the lowest seen
    }

    #[test]
    fn temp_spike_inside_hold_window_resets_it() {
        let mut h = Hysteresis::new(0.0, 3);
        assert_eq!(h.update(70.0, &curve()), 70);
        assert_eq!(h.update(60.0, &curve()), 70); // pending decrease
        assert_eq!(h.update(70.0, &curve()), 70); // spike: pending cleared
        assert_eq!(h.update(60.0, &curve()), 70); // hold starts over
        assert_eq!(h.update(60.0, &curve()), 70);
        assert_eq!(h.update(60.0, &curve()), 50);
    }

    #[test]
    fn reset_clears_state() {
        let mut h = Hysteresis::new(0.0, 5);
        h.update(70.0, &curve());
        h.reset();
        assert_eq!(h.last_output(), None);
        assert_eq!(h.update(55.0, &curve()), 40); // immediate, no history
    }
}
