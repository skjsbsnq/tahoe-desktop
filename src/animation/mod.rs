use std::time::Duration;

use keyframe::functions::{EaseOutCubic, EaseOutQuad};
use keyframe::EasingFunction;

mod bezier;
use bezier::CubicBezier;

mod spring;
pub use spring::{Spring, SpringParams};

mod clock;
pub use clock::Clock;

#[derive(Debug, Clone)]
pub struct Animation {
    from: f64,
    to: f64,
    initial_velocity: f64,
    is_off: bool,
    duration: Duration,
    /// Time until the animation first reaches `to`.
    ///
    /// Best effort; not always exactly precise.
    clamped_duration: Duration,
    start_time: Duration,
    clock: Clock,
    kind: Kind,
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Easing {
        curve: Curve,
    },
    Spring(Spring),
    Deceleration {
        initial_velocity: f64,
        deceleration_rate: f64,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum Curve {
    Linear,
    EaseOutQuad,
    EaseOutCubic,
    EaseOutExpo,
    CubicBezier(CubicBezier),
}

impl Animation {
    pub fn new(
        clock: Clock,
        from: f64,
        to: f64,
        initial_velocity: f64,
        config: niri_config::Animation,
    ) -> Self {
        // Scale the velocity by rate to keep the touchpad gestures feeling right.
        // Callers pass real-time velocity (units per wall-clock second); spring
        // time advances at `clock.rate()`, so convert to animation-time units.
        let initial_velocity = initial_velocity / clock.rate().max(0.001);

        let mut rv = Self::ease(clock, from, to, initial_velocity, 0, Curve::EaseOutCubic);
        if config.off {
            rv.is_off = true;
            return rv;
        }

        rv.replace_config(config);
        rv
    }

    pub fn replace_config(&mut self, config: niri_config::Animation) {
        self.is_off = config.off;
        if config.off {
            self.duration = Duration::ZERO;
            self.clamped_duration = Duration::ZERO;
            return;
        }

        let start_time = self.start_time;

        match config.kind {
            niri_config::animations::Kind::Spring(p) => {
                let params = SpringParams::new(p.damping_ratio, f64::from(p.stiffness), p.epsilon);
                let initial_velocity = Spring::clamp_initial_velocity(
                    self.from,
                    self.to,
                    self.initial_velocity,
                    params,
                );

                let spring = Spring {
                    from: self.from,
                    to: self.to,
                    initial_velocity,
                    params,
                };
                *self = Self::spring(self.clock.clone(), spring);
            }
            niri_config::animations::Kind::Easing(p) => {
                *self = Self::ease(
                    self.clock.clone(),
                    self.from,
                    self.to,
                    self.initial_velocity,
                    u64::from(p.duration_ms),
                    Curve::from(p.curve),
                );
            }
        }

        self.start_time = start_time;
    }

    /// Restarts the animation using the previous config.
    ///
    /// `initial_velocity` is in real-time units (value per wall-clock second),
    /// matching [`Self::velocity`]. Pass the previous animation's velocity to
    /// preserve C1 continuity across retargets.
    pub fn restarted(&self, from: f64, to: f64, initial_velocity: f64) -> Self {
        if self.is_off {
            return self.clone();
        }

        // Scale the velocity by rate to keep the touchpad gestures feeling right.
        let initial_velocity = initial_velocity / self.clock.rate().max(0.001);

        match self.kind {
            Kind::Easing { curve } => Self::ease(
                self.clock.clone(),
                from,
                to,
                initial_velocity,
                self.duration.as_millis() as u64,
                curve,
            ),
            Kind::Spring(spring) => {
                let initial_velocity =
                    Spring::clamp_initial_velocity(from, to, initial_velocity, spring.params);
                let spring = Spring {
                    from,
                    to,
                    initial_velocity,
                    params: spring.params,
                };
                Self::spring(self.clock.clone(), spring)
            }
            Kind::Deceleration {
                initial_velocity: _,
                deceleration_rate,
            } => {
                let threshold = 0.001; // FIXME
                Self::decelerate(
                    self.clock.clone(),
                    from,
                    initial_velocity,
                    deceleration_rate,
                    threshold,
                )
            }
        }
    }

    pub fn ease(
        clock: Clock,
        from: f64,
        to: f64,
        initial_velocity: f64,
        duration_ms: u64,
        curve: Curve,
    ) -> Self {
        let duration = Duration::from_millis(duration_ms);
        let kind = Kind::Easing { curve };

        Self {
            from,
            to,
            initial_velocity,
            is_off: false,
            duration,
            // Our current curves never overshoot.
            clamped_duration: duration,
            start_time: clock.now(),
            clock,
            kind,
        }
    }

    pub fn spring(clock: Clock, mut spring: Spring) -> Self {
        let _span = tracy_client::span!("Animation::spring");

        spring.initial_velocity = Spring::clamp_initial_velocity(
            spring.from,
            spring.to,
            spring.initial_velocity,
            spring.params,
        );

        let duration = spring.duration();
        let clamped_duration = spring.clamped_duration().unwrap_or(duration);
        let from = spring.from;
        let to = spring.to;
        let initial_velocity = spring.initial_velocity;
        let kind = Kind::Spring(spring);

        Self {
            from,
            to,
            initial_velocity,
            is_off: false,
            duration,
            clamped_duration,
            start_time: clock.now(),
            clock,
            kind,
        }
    }

    pub fn decelerate(
        clock: Clock,
        from: f64,
        initial_velocity: f64,
        deceleration_rate: f64,
        threshold: f64,
    ) -> Self {
        let duration_s = if initial_velocity == 0. {
            0.
        } else {
            let coeff = 1000. * deceleration_rate.ln();
            (-coeff * threshold / initial_velocity.abs()).ln() / coeff
        };
        let duration = Duration::from_secs_f64(duration_s);

        let to = from - initial_velocity / (1000. * deceleration_rate.ln());

        let kind = Kind::Deceleration {
            initial_velocity,
            deceleration_rate,
        };

        Self {
            from,
            to,
            initial_velocity,
            is_off: false,
            duration,
            clamped_duration: duration,
            start_time: clock.now(),
            clock,
            kind,
        }
    }

    pub fn is_done(&self) -> bool {
        if self.clock.should_complete_instantly() {
            return true;
        }

        self.clock.now() >= self.start_time + self.duration
    }

    pub fn is_done_with_delay(&self, delay: Duration) -> bool {
        if self.clock.should_complete_instantly() {
            return true;
        }

        self.clock.now() >= self.start_time + delay + self.duration
    }

    pub fn is_clamped_done(&self) -> bool {
        if self.clock.should_complete_instantly() {
            return true;
        }

        self.clock.now() >= self.start_time + self.clamped_duration
    }

    pub fn value_at(&self, at: Duration) -> f64 {
        if at <= self.start_time {
            // Return from when at == start_time so that when the animations are off, the behavior
            // within a single event loop cycle (i.e. no time had passed since the start of an
            // animation) matches the behavior when the animations are on.
            return self.from;
        } else if self.start_time + self.duration <= at {
            return self.to;
        }

        if self.clock.should_complete_instantly() {
            return self.to;
        }

        let passed = at.saturating_sub(self.start_time);

        match self.kind {
            Kind::Easing { curve } => {
                let passed = passed.as_secs_f64();
                let total = self.duration.as_secs_f64();
                let x = (passed / total).clamp(0., 1.);
                curve.y(x) * (self.to - self.from) + self.from
            }
            Kind::Spring(spring) => {
                let value = spring.value_at(passed);

                // Protect against numerical instability.
                let range = (self.to - self.from) * 10.;
                let a = self.from - range;
                let b = self.to + range;
                if self.from <= self.to {
                    value.clamp(a, b)
                } else {
                    value.clamp(b, a)
                }
            }
            Kind::Deceleration {
                initial_velocity,
                deceleration_rate,
            } => {
                let passed = passed.as_secs_f64();
                let coeff = 1000. * deceleration_rate.ln();
                self.from + (deceleration_rate.powf(1000. * passed) - 1.) / coeff * initial_velocity
            }
        }
    }

    pub fn value(&self) -> f64 {
        self.value_at(self.clock.now())
    }

    /// Current velocity in real-time units (value per wall-clock second).
    ///
    /// Spring uses the closed-form derivative; easing and deceleration use a
    /// 1 ms finite difference. The return value is multiplied back by
    /// `clock.rate()` so it matches the real-time velocity that
    /// [`Self::new`] / [`Self::restarted`] expect as `initial_velocity`
    /// (those divide by rate on the way in — without the multiply, slow-mo
    /// debugging would double-scale).
    pub fn velocity(&self) -> f64 {
        self.velocity_at(self.clock.now())
    }

    /// Velocity at an absolute clock time. See [`Self::velocity`].
    pub fn velocity_at(&self, at: Duration) -> f64 {
        if self.is_off || self.clock.should_complete_instantly() {
            return 0.;
        }

        // Frozen clock (rate 0): wall-clock time does not advance, so the
        // real-time velocity is zero. (Animation-time derivative still exists
        // internally; callers under a frozen clock should not hand off via
        // velocity() → restarted.)
        let rate = self.clock.rate();
        if rate <= 0. {
            return 0.;
        }

        // Not yet started, or fully elapsed → settled at rest.
        // At exactly `start_time` we still report the initial velocity so a
        // just-restarted animation hands off C1-continuously.
        if at < self.start_time || at >= self.start_time + self.duration {
            return 0.;
        }

        let anim_velocity = match self.kind {
            Kind::Spring(spring) => {
                let passed = at.saturating_sub(self.start_time);
                spring.velocity_at(passed)
            }
            Kind::Easing { .. } | Kind::Deceleration { .. } => {
                // 1 ms finite difference in animation time.
                let dt = Duration::from_millis(1);
                let end = self.start_time + self.duration;
                let t0 = at.saturating_sub(dt).max(self.start_time);
                let t1 = at.saturating_add(dt).min(end);
                let denom = t1.saturating_sub(t0).as_secs_f64();
                if denom <= f64::EPSILON {
                    0.
                } else {
                    (self.value_at(t1) - self.value_at(t0)) / denom
                }
            }
        };

        // Convert animation-time velocity → real-time velocity.
        // Symmetric with new/restarted which divide by rate.max(0.001); here
        // rate is already > 0 from the early return above.
        anim_velocity * rate
    }

    /// Returns a value that stops at the target value after first reaching it.
    ///
    /// Best effort; not always exactly precise.
    pub fn clamped_value(&self) -> f64 {
        if self.is_clamped_done() {
            return self.to;
        }

        self.value()
    }

    pub fn clamped_value_with_delay(&self, delay: Duration) -> f64 {
        if self.clock.should_complete_instantly() {
            return self.to;
        }

        let now = self.clock.now();
        let delayed_start = self.start_time + delay;
        if now < delayed_start {
            return self.from;
        }

        if now >= delayed_start + self.clamped_duration {
            return self.to;
        }

        self.value_at(now - delay)
    }

    pub fn to(&self) -> f64 {
        self.to
    }

    pub fn from(&self) -> f64 {
        self.from
    }

    pub fn start_time(&self) -> Duration {
        self.start_time
    }

    pub fn end_time(&self) -> Duration {
        self.start_time + self.duration
    }

    pub fn duration(&self) -> Duration {
        self.duration
    }

    pub fn offset(&mut self, offset: f64) {
        self.from += offset;
        self.to += offset;

        if let Kind::Spring(spring) = &mut self.kind {
            spring.from += offset;
            spring.to += offset;
        }
    }
}

impl Curve {
    pub fn y(self, x: f64) -> f64 {
        match self {
            Curve::Linear => x,
            Curve::EaseOutQuad => EaseOutQuad.y(x),
            Curve::EaseOutCubic => EaseOutCubic.y(x),
            Curve::EaseOutExpo => 1. - 2f64.powf(-10. * x),
            Curve::CubicBezier(b) => b.y(x),
        }
    }
}

impl From<niri_config::animations::Curve> for Curve {
    fn from(value: niri_config::animations::Curve) -> Self {
        match value {
            niri_config::animations::Curve::Linear => Curve::Linear,
            niri_config::animations::Curve::EaseOutQuad => Curve::EaseOutQuad,
            niri_config::animations::Curve::EaseOutCubic => Curve::EaseOutCubic,
            niri_config::animations::Curve::EaseOutExpo => Curve::EaseOutExpo,
            niri_config::animations::Curve::CubicBezier(x1, y1, x2, y2) => {
                Curve::CubicBezier(CubicBezier::new(x1, y1, x2, y2))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tahoe_safe_cubic_beziers_sample_without_nan_or_backtracking() {
        let curves = [
            ("emphasized-decel", CubicBezier::new(0.05, 0.7, 0.1, 1.)),
            ("emphasized-accel", CubicBezier::new(0.3, 0., 0.8, 0.15)),
            ("standard-decel", CubicBezier::new(0., 0., 0., 1.)),
            ("expressive-effects", CubicBezier::new(0.34, 0.8, 0.34, 1.)),
            ("menu-decel-safe", CubicBezier::new(0.12, 0.95, 0.16, 1.)),
            ("menu-accel", CubicBezier::new(0.52, 0.03, 0.72, 0.08)),
        ];

        for (name, bezier) in curves {
            let curve = Curve::CubicBezier(bezier);
            let mut previous = curve.y(0.);

            for step in 1..=100 {
                let x = f64::from(step) / 100.;
                let y = curve.y(x);

                assert!(y.is_finite(), "{name} returned non-finite y at x={x}");
                assert!(
                    y + 0.001 >= previous,
                    "{name} moved backwards at x={x}: {y} < {previous}"
                );

                previous = y;
            }
        }
    }

    #[test]
    fn zero_duration_delayed_clamped_value_reaches_target_at_start() {
        let clock = Clock::with_time(Duration::ZERO);
        let animation = Animation::ease(clock.clone(), 0., 1., 0., 0, Curve::Linear);

        assert_eq!(animation.clamped_value_with_delay(Duration::ZERO), 1.);
    }

    #[test]
    fn zero_duration_delayed_clamped_value_waits_for_delay() {
        let mut clock = Clock::with_time(Duration::ZERO);
        let animation = Animation::ease(clock.clone(), 0., 1., 0., 0, Curve::Linear);

        assert_eq!(
            animation.clamped_value_with_delay(Duration::from_millis(1)),
            0.
        );
        clock.set_unadjusted(Duration::from_millis(1));
        assert_eq!(
            animation.clamped_value_with_delay(Duration::from_millis(1)),
            1.
        );
    }

    #[test]
    fn restarted_spring_uses_new_initial_velocity() {
        let clock = Clock::with_time(Duration::ZERO);
        let animation = Animation::spring(
            clock,
            Spring {
                from: 0.,
                to: 1.,
                initial_velocity: 3.,
                params: SpringParams::new(0.8, 500., 0.001),
            },
        );

        let restarted = animation.restarted(0.4, 0., -7.);

        assert_eq!(restarted.initial_velocity, -7.);
        let Kind::Spring(spring) = restarted.kind else {
            panic!("restarted spring changed animation kind");
        };
        assert_eq!(spring.initial_velocity, -7.);

        let sample_time = Duration::from_micros(100);
        let sampled_velocity = (restarted.value_at(sample_time)
            - restarted.value_at(Duration::ZERO))
            / sample_time.as_secs_f64();
        assert!((sampled_velocity - -7.).abs() < 0.1);
    }

    #[test]
    fn spring_velocity_matches_numerical_derivative() {
        // Spring::velocity_at is checked vs oscillate at <1e-6 in spring tests.
        // Here: Animation::velocity_at must equal spring derivative × clock.rate.
        let clock = Clock::with_time(Duration::ZERO);
        let animation = Animation::spring(
            clock.clone(),
            Spring {
                from: 0.,
                to: 200.,
                initial_velocity: 0.,
                params: SpringParams::new(0.85, 600., 0.0001),
            },
        );

        let Kind::Spring(spring) = animation.kind else {
            panic!("expected spring");
        };

        for millis in [10u64, 30, 80, 150] {
            let at = Duration::from_millis(millis);
            let got = animation.velocity_at(at);
            let expect = spring.velocity_at(at) * clock.rate().max(0.001);

            // Also cross-check spring analytical vs value_at central difference.
            let h = Duration::from_nanos(100);
            let numerical = (spring.value_at(at + h) - spring.value_at(at.saturating_sub(h)))
                / (2. * h.as_secs_f64());

            assert!(
                (got - expect).abs() < 1e-12,
                "@{millis}ms wrapper={got} expect={expect}"
            );
            assert!(
                (spring.velocity_at(at) - numerical).abs() < 1e-6,
                "@{millis}ms analytical={} numerical={numerical}",
                spring.velocity_at(at)
            );
        }
    }

    #[test]
    fn velocity_scales_with_clock_rate_no_double_scale() {
        // Build two identical springs on clocks at different rates. After the
        // same *animation* progress, the real-time velocity reported at rate r
        // must equal rate-1 velocity × r (velocity() multiplies rate back out
        // so callers can feed it straight into restarted/new).
        let mut clock_r1 = Clock::with_time(Duration::ZERO);
        clock_r1.set_rate(1.0);
        let anim_r1 = Animation::spring(
            clock_r1.clone(),
            Spring {
                from: 0.,
                to: 1.,
                initial_velocity: 0.,
                params: SpringParams::new(0.8, 500., 0.001),
            },
        );

        let mut clock_r05 = Clock::with_time(Duration::ZERO);
        clock_r05.set_rate(0.5);
        // Same spring numbers; animation-time derivative is identical when the
        // spring itself is identical. velocity() then × rate.
        let anim_r05 = Animation::spring(
            clock_r05.clone(),
            Spring {
                from: 0.,
                to: 1.,
                initial_velocity: 0.,
                params: SpringParams::new(0.8, 500., 0.001),
            },
        );

        // Advance both clocks so their *adjusted* now equals 100ms of spring
        // time: rate 1 → unadjusted 100ms; rate 0.5 → unadjusted 200ms.
        clock_r1.set_unadjusted(Duration::from_millis(100));
        clock_r05.set_unadjusted(Duration::from_millis(200));

        assert_eq!(clock_r1.now(), Duration::from_millis(100));
        assert_eq!(clock_r05.now(), Duration::from_millis(100));

        let v1 = anim_r1.velocity();
        let v05 = anim_r05.velocity();

        // Real-time velocity at half rate is half (spring progresses half as
        // fast in wall time). Must NOT be quartered (double-scale bug).
        assert!(
            (v05 - 0.5 * v1).abs() < 1e-9,
            "rate scaling broken: v1={v1} v05={v05} (want 0.5*v1)"
        );
        assert!(v1.abs() > 1e-6, "spring should be moving at 100ms");
    }

    #[test]
    fn velocity_round_trips_through_restarted() {
        let mut clock = Clock::with_time(Duration::ZERO);
        let anim = Animation::spring(
            clock.clone(),
            Spring {
                from: 0.,
                to: 100.,
                initial_velocity: 0.,
                params: SpringParams::new(0.9, 700., 0.0005),
            },
        );

        clock.set_unadjusted(Duration::from_millis(80));
        let before = anim.value();
        let vel = anim.velocity();
        assert!(vel.abs() > 1e-3, "expected nonzero mid-flight velocity");

        let restarted = anim.restarted(before, 50., vel);

        // At the restart instant, velocity() must equal the handed-off velocity
        // (acceptance: restart 前后速度差 < 1%).
        let handed = restarted.velocity_at(restarted.start_time());
        assert!(
            (handed - vel).abs() / vel.abs().max(1.) < 0.01,
            "velocity handoff: handed={handed} want≈{vel}"
        );
        // Storage is animation-time units (= real-time / rate); rate is 1 here.
        assert!((restarted.initial_velocity - vel / clock.rate().max(0.001)).abs() < 1e-9);
    }

    #[test]
    fn new_and_restarted_rate_divide_round_trips() {
        // Exercise the divide-on-input path (Animation::new / restarted) at
        // rate ≠ 1, complementing the bare-spring multiply-out test.
        let mut clock = Clock::with_time(Duration::ZERO);
        clock.set_rate(0.5);

        let config = niri_config::Animation {
            off: false,
            kind: niri_config::animations::Kind::Spring(niri_config::animations::SpringParams {
                damping_ratio: 0.85,
                stiffness: 600,
                epsilon: 0.0001,
            }),
        };

        // Pass real-time velocity 10; internally stored as 10/0.5 = 20.
        let anim = Animation::new(clock.clone(), 0., 100., 10., config);
        assert!((anim.initial_velocity - 20.).abs() < 1e-9);
        // velocity() at t=0 multiplies rate back → 20 * 0.5 = 10.
        assert!((anim.velocity_at(anim.start_time()) - 10.).abs() < 1e-9);

        let restarted = anim.restarted(40., 0., 10.);
        assert!((restarted.initial_velocity - 20.).abs() < 1e-9);
        assert!((restarted.velocity_at(restarted.start_time()) - 10.).abs() < 1e-9);
    }

    #[test]
    fn easing_velocity_finite_difference() {
        let mut clock = Clock::with_time(Duration::ZERO);
        let anim = Animation::ease(clock.clone(), 0., 100., 0., 200, Curve::Linear);

        clock.set_unadjusted(Duration::from_millis(50));
        // Linear 0→100 over 200ms → 500 units/s.
        let v = anim.velocity();
        assert!(
            (v - 500.).abs() < 1.0,
            "linear easing velocity want≈500 got {v}"
        );
    }

    /// T-10 acceptance: retarget via `Animation::new(..., old.velocity())`
    /// keeps velocity continuous (diff < 1%).
    ///
    /// Instantaneous `velocity()` is the C1 contract. A 1 ms finite-difference
    /// sample is also checked with a tight step: target change makes
    /// *acceleration* discontinuous, so a full 1 ms average after retarget
    /// legitimately drifts; we probe with 100 µs so the average stays within
    /// 1% of the handed-off instantaneous velocity.
    #[test]
    fn retarget_via_new_preserves_velocity_within_one_percent() {
        let mut clock = Clock::with_time(Duration::ZERO);
        let config = niri_config::Animation {
            off: false,
            kind: niri_config::animations::Kind::Spring(niri_config::animations::SpringParams {
                damping_ratio: 1.0,
                stiffness: 1000,
                epsilon: 0.0001,
            }),
        };

        // Horizontal-view-style: animate 0 → 400.
        let anim = Animation::new(clock.clone(), 0., 400., 0., config);
        clock.set_unadjusted(Duration::from_millis(60));

        let before_pos = anim.value();
        let before_vel = anim.velocity();
        assert!(before_vel.abs() > 1., "spring should be moving at 60ms");

        // Retarget to a new column offset, handing off velocity (T-10 pattern).
        let retargeted = Animation::new(clock.clone(), before_pos, 120., before_vel, config);

        // Instantaneous API must agree within 1%.
        let handoff = retargeted.velocity_at(retargeted.start_time());
        assert!(
            (handoff - before_vel).abs() / before_vel.abs().max(1.) < 0.01,
            "velocity() handoff: {handoff} vs {before_vel}"
        );

        // Tight FD just after restart ≈ instantaneous handoff (C1, not C2).
        let dt = Duration::from_micros(100);
        let fd_after = (retargeted.value_at(retargeted.start_time() + dt)
            - retargeted.value_at(retargeted.start_time()))
            / dt.as_secs_f64();
        let rel = (fd_after - before_vel).abs() / before_vel.abs().max(1.);
        assert!(
            rel < 0.01,
            "post-retarget FD velocity drifted: fd={fd_after} want≈{before_vel} rel={rel}"
        );
    }

    /// T-10 tile/mru move pattern: normalized 1→0 anim, v_norm = v_abs / new_from.
    #[test]
    fn normalized_move_velocity_preserves_absolute_offset_rate() {
        let mut clock = Clock::with_time(Duration::ZERO);
        let config = niri_config::Animation {
            off: false,
            kind: niri_config::animations::Kind::Spring(niri_config::animations::SpringParams {
                damping_ratio: 1.0,
                stiffness: 800,
                epsilon: 0.0001,
            }),
        };

        // First move: from=200, anim 1→0 ⇒ offset = 200 * value.
        let anim = Animation::new(clock.clone(), 1., 0., 0., config);
        let old_from = 200.;
        clock.set_unadjusted(Duration::from_millis(40));

        let abs_vel = old_from * anim.velocity();
        assert!(abs_vel.abs() > 1., "absolute offset should be moving");

        // Chain another move that extends the visual offset.
        let current_offset = old_from * anim.value();
        let extra = 80.;
        let new_from = extra + current_offset;
        let v_norm = abs_vel / new_from;
        let retargeted = anim.restarted(1., 0., v_norm);

        // Absolute offset velocity after retarget = new_from * v_norm ≈ abs_vel.
        let abs_after = new_from * retargeted.velocity_at(retargeted.start_time());
        let rel = (abs_after - abs_vel).abs() / abs_vel.abs().max(1.);
        assert!(
            rel < 0.01,
            "normalized move lost absolute velocity: before={abs_vel} after={abs_after}"
        );
    }
}
