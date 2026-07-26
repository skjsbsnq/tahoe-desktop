//! Compositor-side presentation-transform animation for tahoe-glass surfaces.
//!
//! Reuses the [`OpenAnimation`](super::opening_layer::OpenAnimation) skeleton:
//! a single progress [`Animation`] (spring or cubic-bezier) drives a linear
//! interpolation between two affines, which the render path maps onto
//! `RescaleRenderElement`/`RelocateRenderElement` wrappers. With shared curve
//! parameters per channel a single progress is equivalent to per-channel
//! springs while keeping mid-flight retargeting simple.

use std::time::Duration;

use crate::animation::{Animation, Clock, Curve, Spring, SpringParams};
use crate::protocols::tahoe_glass::{PresentationAffine, TahoeTransformCurve};

#[derive(Debug)]
pub struct PresentationTransformAnimation {
    /// Unclamped 0→1 progress. Springs are allowed to overshoot so the
    /// compositor-side morph keeps the bouncy settle of the client-side
    /// animations it replaces; the affine lerp guards scale positivity.
    progress: Animation,
    from: PresentationAffine,
    to: PresentationAffine,
    clock: Clock,
}

impl PresentationTransformAnimation {
    pub fn new(
        clock: Clock,
        from: PresentationAffine,
        to: PresentationAffine,
        initial_velocity: f64,
        curve: TahoeTransformCurve,
    ) -> Self {
        let progress = match curve {
            TahoeTransformCurve::Spring {
                damping_ratio,
                stiffness,
                epsilon,
            } => Animation::spring(
                clock.clone(),
                Spring {
                    from: 0.,
                    to: 1.,
                    initial_velocity,
                    params: SpringParams::new(damping_ratio, stiffness, epsilon),
                },
            ),
            TahoeTransformCurve::Eased {
                duration_ms,
                bezier,
            } => Animation::ease(
                clock.clone(),
                0.,
                1.,
                initial_velocity,
                u64::from(duration_ms),
                Curve::from(niri_config::animations::Curve::CubicBezier(
                    bezier.0, bezier.1, bezier.2, bezier.3,
                )),
            ),
        };

        Self {
            progress,
            from,
            to,
            clock,
        }
    }

    pub fn is_done(&self) -> bool {
        self.progress.is_done()
    }

    pub fn to(&self) -> PresentationAffine {
        self.to
    }

    /// Current interpolated affine. Spring overshoot deliberately passes
    /// through unclamped; scales are floored to stay positive.
    pub fn current(&self) -> PresentationAffine {
        if self.progress.is_done() {
            return self.to;
        }

        lerp_affine(self.from, self.to, self.progress.value())
    }

    /// Progress velocity right now, in progress units per second.
    fn progress_velocity(&self) -> f64 {
        let now = self.clock.now();
        let dt = Duration::from_micros(1000);
        let earlier = now.saturating_sub(dt);
        if now == earlier {
            return 0.;
        }

        (self.progress.value_at(now) - self.progress.value_at(earlier)) / dt.as_secs_f64()
    }

    /// Initial progress velocity for a retargeted animation, projecting the
    /// current channel velocities onto the new from→to direction
    /// (least-squares). Channels share one progress, so for a parallel
    /// retarget this reduces to the plain progress velocity.
    pub fn velocity_toward(
        &self,
        new_from: &PresentationAffine,
        new_to: &PresentationAffine,
    ) -> f64 {
        let vp = self.progress_velocity();
        if vp == 0. {
            return 0.;
        }

        let old = channel_deltas(&self.from, &self.to);
        let new = channel_deltas(new_from, new_to);
        let num: f64 = old.iter().zip(&new).map(|(o, n)| vp * o * n).sum();
        let den: f64 = new.iter().map(|n| n * n).sum();
        if den <= f64::EPSILON {
            0.
        } else {
            num / den
        }
    }
}

fn channel_deltas(from: &PresentationAffine, to: &PresentationAffine) -> [f64; 4] {
    [
        to.x - from.x,
        to.y - from.y,
        to.scale_x - from.scale_x,
        to.scale_y - from.scale_y,
    ]
}

fn lerp_affine(from: PresentationAffine, to: PresentationAffine, t: f64) -> PresentationAffine {
    PresentationAffine {
        x: from.x + (to.x - from.x) * t,
        y: from.y + (to.y - from.y) * t,
        scale_x: (from.scale_x + (to.scale_x - from.scale_x) * t).max(0.01),
        scale_y: (from.scale_y + (to.scale_y - from.scale_y) * t).max(0.01),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn affine(x: f64, y: f64, scale_x: f64, scale_y: f64) -> PresentationAffine {
        PresentationAffine {
            x,
            y,
            scale_x,
            scale_y,
        }
    }

    fn spring_curve() -> TahoeTransformCurve {
        TahoeTransformCurve::Spring {
            damping_ratio: 0.85,
            stiffness: 160.,
            epsilon: 0.001,
        }
    }

    #[test]
    fn animation_starts_at_from_and_settles_at_to() {
        let mut clock = Clock::with_time(Duration::ZERO);
        let from = affine(120., 4., 0.3, 0.2);
        let to = PresentationAffine::IDENTITY;
        let anim = PresentationTransformAnimation::new(clock.clone(), from, to, 0., spring_curve());

        assert_eq!(anim.current(), from);

        clock.set_unadjusted(Duration::from_secs(30));
        assert!(anim.is_done());
        assert_eq!(anim.current(), to);
    }

    #[test]
    fn eased_zero_duration_completes_instantly() {
        let mut clock = Clock::with_time(Duration::ZERO);
        let from = affine(10., 0., 1., 1.);
        let to = PresentationAffine::IDENTITY;
        let anim = PresentationTransformAnimation::new(
            clock.clone(),
            from,
            to,
            0.,
            TahoeTransformCurve::Eased {
                duration_ms: 0,
                bezier: (0.215, 0.61, 0.355, 1.),
            },
        );

        clock.set_unadjusted(Duration::from_millis(1));
        assert!(anim.is_done());
        assert_eq!(anim.current(), to);
    }

    #[test]
    fn lerp_guards_scale_positivity_under_extreme_overshoot() {
        let from = affine(0., 0., 0.5, 0.5);
        let to = affine(0., 0., 1., 1.);
        // A wildly out-of-range progress must never produce non-positive scale.
        let value = lerp_affine(from, to, -30.);
        assert!(value.scale_x >= 0.01);
        assert!(value.scale_y >= 0.01);
    }

    #[test]
    fn velocity_projection_is_identity_for_parallel_retarget() {
        let mut clock = Clock::with_time(Duration::ZERO);
        let from = affine(100., 0., 1., 1.);
        let to = PresentationAffine::IDENTITY;
        let anim = PresentationTransformAnimation::new(clock.clone(), from, to, 0., spring_curve());

        clock.set_unadjusted(Duration::from_millis(120));
        let vp = anim.progress_velocity();
        assert!(vp > 0., "spring should be moving after 120ms");

        // Same direction → projected velocity equals the progress velocity.
        let projected = anim.velocity_toward(&from, &to);
        assert!((projected - vp).abs() < 1e-9);

        // Reversed direction → projected velocity flips sign.
        let reversed = anim.velocity_toward(&to, &from);
        assert!((reversed + vp).abs() < 1e-9);
    }
}
