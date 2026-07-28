use std::collections::VecDeque;
use std::time::Duration;

const HISTORY_LIMIT: Duration = Duration::from_millis(150);
const DECELERATION_TOUCHPAD: f64 = 0.997;

#[derive(Debug)]
pub struct SwipeTracker {
    history: VecDeque<Event>,
    pos: f64,
}

#[derive(Debug, Clone, Copy)]
struct Event {
    delta: f64,
    timestamp: Duration,
}

impl SwipeTracker {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            history: VecDeque::new(),
            pos: 0.,
        }
    }

    /// Pushes a new reading into the tracker.
    pub fn push(&mut self, delta: f64, timestamp: Duration) {
        // For the events that we care about, timestamps should always increase
        // monotonically.
        if let Some(last) = self.history.back() {
            if timestamp < last.timestamp {
                trace!(
                    "ignoring event with timestamp {timestamp:?} earlier than last {:?}",
                    last.timestamp
                );
                return;
            }
        }

        self.history.push_back(Event { delta, timestamp });
        self.pos += delta;

        self.trim_history();
    }

    /// Returns the current gesture position.
    pub fn pos(&self) -> f64 {
        self.pos
    }

    /// Timestamp of the most recent accepted event, if any.
    pub fn last_timestamp(&self) -> Option<Duration> {
        self.history.back().map(|e| e.timestamp)
    }

    /// Computes the current gesture velocity via least-squares slope of
    /// cumulative position versus time (GNOME Shell / Clutter style).
    ///
    /// The previous average `Σdelta / (t_last − t_first)` counted the first
    /// event's delta in the numerator while excluding the time interval that
    /// produced it, systematically overestimating sparse 2–4 event flings by
    /// 30–100%. Linear regression on cumulative position excludes that bias:
    /// for two events the slope is simply `δ₁ / (t₁ − t₀)`.
    pub fn velocity(&self) -> f64 {
        let n = self.history.len();
        if n < 2 {
            return 0.;
        }

        // Relative times keep the normal equations well-conditioned for both
        // synthetic test clocks (ms-scale) and real monotonic uptime.
        let t0 = self.history.front().unwrap().timestamp;

        let mut sum_t = 0.0_f64;
        let mut sum_p = 0.0_f64;
        let mut sum_tt = 0.0_f64;
        let mut sum_tp = 0.0_f64;
        let mut pos = 0.0_f64;

        for event in &self.history {
            pos += event.delta;
            let t = (event.timestamp - t0).as_secs_f64();
            sum_t += t;
            sum_p += pos;
            sum_tt += t * t;
            sum_tp += t * pos;
        }

        let n_f = n as f64;
        let denom = n_f * sum_tt - sum_t * sum_t;
        if denom.abs() < 1e-18 {
            return 0.;
        }

        (n_f * sum_tp - sum_t * sum_p) / denom
    }

    /// Computes the gesture end position after decelerating to a halt.
    pub fn projected_end_pos(&self) -> f64 {
        let vel = self.velocity();
        self.pos - vel / (1000. * DECELERATION_TOUCHPAD.ln())
    }

    fn trim_history(&mut self) {
        let Some(&Event { timestamp, .. }) = self.history.back() else {
            return;
        };

        while let Some(first) = self.history.front() {
            if timestamp <= first.timestamp + HISTORY_LIMIT {
                break;
            }

            // Dropping an old event must not change `pos` (absolute position
            // since gesture begin); only the velocity window shrinks.
            let _ = self.history.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    /// T-14 acceptance: a 2-event sequence must report the inter-event slope,
    /// not the biased average that included the first delta without its time.
    #[test]
    fn two_event_velocity_is_inter_event_slope() {
        let mut t = SwipeTracker::new();
        t.push(100., ms(0));
        t.push(50., ms(10));

        // Old formula: (100+50)/0.01 = 15000. Correct slope: 50/0.01 = 5000.
        let v = t.velocity();
        assert!(
            (v - 5000.).abs() < 1e-6,
            "2-event velocity must be δ1/Δt, got {v}"
        );
    }

    #[test]
    fn two_event_velocity_zero_when_same_timestamp() {
        let mut t = SwipeTracker::new();
        t.push(10., ms(5));
        t.push(20., ms(5));
        assert_eq!(t.velocity(), 0.);
    }

    #[test]
    fn single_event_velocity_is_zero() {
        let mut t = SwipeTracker::new();
        t.push(40., ms(0));
        assert_eq!(t.velocity(), 0.);
    }

    #[test]
    fn three_event_uniform_motion_matches_slope() {
        let mut t = SwipeTracker::new();
        // Constant 1000 px/s: +10 every 10ms.
        t.push(10., ms(0));
        t.push(10., ms(10));
        t.push(10., ms(20));
        let v = t.velocity();
        assert!(
            (v - 1000.).abs() < 1e-6,
            "uniform motion LS slope should be 1000, got {v}"
        );
    }

    #[test]
    fn idle_zero_delta_decays_velocity() {
        let mut t = SwipeTracker::new();
        t.push(50., ms(0));
        t.push(50., ms(10));
        let before = t.velocity();
        assert!(before > 0.);

        // Hold still for 100ms — LS through a flat tail must shrink velocity.
        t.push(0., ms(110));
        let after = t.velocity();
        assert!(
            after < before * 0.5,
            "idle compensation must decay velocity: before={before} after={after}"
        );
    }

    #[test]
    fn earliest_timestamp_rejected_does_not_poison_history() {
        let mut t = SwipeTracker::new();
        t.push(10., ms(20));
        t.push(10., ms(30));
        // Stale "now" earlier than last event — must be ignored (production bug
        // when lazy clock latched before the last libinput timestamp).
        t.push(0., ms(15));
        assert_eq!(t.history.len(), 2);
        assert!((t.velocity() - 1000.).abs() < 1e-6);
    }

    /// Old average vs LS: sparse fling overestimate factor on a classic 3-event
    /// burst where the first delta is large.
    #[test]
    fn negative_deltas_yield_signed_slope() {
        let mut t = SwipeTracker::new();
        t.push(-80., ms(0));
        t.push(-40., ms(20));
        let v = t.velocity();
        assert!(
            (v - (-2000.)).abs() < 1e-6,
            "leftward 2-event slope must be -2000, got {v}"
        );
    }

    #[test]
    fn sparse_fling_no_longer_overestimates_first_delta() {
        let mut t = SwipeTracker::new();
        t.push(300., ms(0));
        t.push(40., ms(10));
        t.push(50., ms(20));
        t.push(60., ms(30));

        let v = t.velocity();
        // Old: (300+40+50+60)/0.03 = 15000.
        // LS on cumulative (300,340,390,450) @ (0,10,20,30)ms:
        //   n=4, t=(0,0.01,0.02,0.03), p=(300,340,390,450)
        //   slope = (n*Σtp - Σt*Σp) / (n*Σtt - (Σt)²)
        let times = [0.0, 0.01, 0.02, 0.03];
        let poss = [300.0, 340.0, 390.0, 450.0];
        let n = 4.0;
        let sum_t: f64 = times.iter().sum();
        let sum_p: f64 = poss.iter().sum();
        let sum_tt: f64 = times.iter().map(|t| t * t).sum();
        let sum_tp: f64 = times.iter().zip(poss).map(|(t, p)| t * p).sum();
        let expected = (n * sum_tp - sum_t * sum_p) / (n * sum_tt - sum_t * sum_t);

        assert!(
            (v - expected).abs() < 1e-6,
            "LS velocity mismatch: got {v} want {expected}"
        );
        // Must be substantially below the old biased average.
        assert!(
            v < 12000.,
            "T-14 must de-bias first-delta overestimate, got {v}"
        );
    }
}
