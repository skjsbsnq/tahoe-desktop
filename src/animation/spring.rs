use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct SpringParams {
    pub damping: f64,
    pub mass: f64,
    pub stiffness: f64,
    pub epsilon: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct Spring {
    pub from: f64,
    pub to: f64,
    pub initial_velocity: f64,
    pub params: SpringParams,
}

impl SpringParams {
    pub fn new(damping_ratio: f64, stiffness: f64, epsilon: f64) -> Self {
        let damping_ratio = damping_ratio.max(0.);
        let stiffness = stiffness.max(0.);
        let epsilon = epsilon.max(0.);

        let mass = 1.;
        let critical_damping = 2. * (mass * stiffness).sqrt();
        let damping = damping_ratio * critical_damping;

        Self {
            damping,
            mass,
            stiffness,
            epsilon,
        }
    }
}

impl Spring {
    pub fn value_at(&self, t: Duration) -> f64 {
        self.oscillate(t.as_secs_f64())
    }

    // Based on libadwaita (LGPL-2.1-or-later):
    // https://gitlab.gnome.org/GNOME/libadwaita/-/blob/1.4.4/src/adw-spring-animation.c,
    // which itself is based on (MIT):
    // https://github.com/robb/RBBAnimation/blob/master/RBBAnimation/RBBSpringAnimation.m
    /// Computes and returns the duration until the spring is at rest.
    pub fn duration(&self) -> Duration {
        const DELTA: f64 = 0.001;
        const MAX_ITERATIONS: usize = 1000;

        let beta = self.params.damping / (2. * self.params.mass);

        if beta.abs() <= f64::EPSILON || beta < 0. {
            return Duration::MAX;
        }

        if (self.to - self.from).abs() <= f64::EPSILON {
            return Duration::ZERO;
        }

        let omega0 = (self.params.stiffness / self.params.mass).sqrt();

        // As first ansatz for the overdamped solution,
        // and general estimation for the oscillating ones
        // we take the value of the envelope when it's < epsilon.
        let mut x0 = -self.params.epsilon.ln() / beta;

        // f64::EPSILON is too small for this specific comparison, so we use
        // f32::EPSILON even though it's doubles.
        if (beta - omega0).abs() <= f64::from(f32::EPSILON) || beta < omega0 {
            return Duration::from_secs_f64(x0);
        }

        let fallback_guess = x0;
        let fallback = || self.overdamped_duration_fallback(beta, omega0, fallback_guess);

        // Since the overdamped solution decays way slower than the envelope
        // we need to use the value of the oscillation itself.
        // Newton's root finding method is a good candidate in this particular case:
        // https://en.wikipedia.org/wiki/Newton%27s_method
        let mut y0 = self.oscillate(x0);
        let m = (self.oscillate(x0 + DELTA) - y0) / DELTA;
        if !m.is_finite() || m.abs() <= f64::EPSILON {
            return fallback();
        }

        let mut x1 = (self.to - y0 + m * x0) / m;
        if !x1.is_finite() || x1 < 0. {
            return fallback();
        }
        let mut y1 = self.oscillate(x1);
        if !y1.is_finite() {
            return fallback();
        }

        let mut iterations = 0;
        while (self.to - y1).abs() > self.params.epsilon {
            if iterations >= MAX_ITERATIONS {
                return fallback();
            }

            x0 = x1;
            y0 = y1;

            let m = (self.oscillate(x0 + DELTA) - y0) / DELTA;
            if !m.is_finite() || m.abs() <= f64::EPSILON {
                return fallback();
            }

            x1 = (self.to - y0 + m * x0) / m;
            if !x1.is_finite() || x1 < 0. {
                return fallback();
            }
            y1 = self.oscillate(x1);

            if !y1.is_finite() {
                return fallback();
            }

            iterations += 1;
        }

        Duration::from_secs_f64(x1)
    }

    fn overdamped_duration_fallback(&self, beta: f64, omega0: f64, initial_guess: f64) -> Duration {
        let omega2 = ((beta * beta) - (omega0 * omega0)).sqrt();
        let slow_rate = -beta + omega2;
        let fast_rate = -beta - omega2;
        let displacement = self.from - self.to;
        let denominator = slow_rate - fast_rate;
        let slow_coefficient = (self.initial_velocity - fast_rate * displacement) / denominator;
        let fast_coefficient = displacement - slow_coefficient;
        let bound = |time: f64| {
            slow_coefficient.abs() * (slow_rate * time).exp()
                + fast_coefficient.abs() * (fast_rate * time).exp()
        };

        let mut upper = initial_guess.max(0.001);
        for _ in 0..128 {
            let error_bound = bound(upper);
            if error_bound.is_finite() && error_bound <= self.params.epsilon {
                let mut lower = 0.;
                for _ in 0..64 {
                    let middle = (lower + upper) / 2.;
                    if bound(middle) <= self.params.epsilon {
                        upper = middle;
                    } else {
                        lower = middle;
                    }
                }

                return Duration::from_secs_f64(upper).max(Duration::from_millis(1));
            }

            upper *= 2.;
            if !upper.is_finite() {
                return Duration::MAX;
            }
        }

        Duration::MAX
    }

    /// Computes and returns the duration until the spring reaches its target position.
    pub fn clamped_duration(&self) -> Option<Duration> {
        let beta = self.params.damping / (2. * self.params.mass);

        if beta.abs() <= f64::EPSILON || beta < 0. {
            return Some(Duration::MAX);
        }

        if (self.to - self.from).abs() <= f64::EPSILON {
            return Some(Duration::ZERO);
        }

        // The first frame is not that important and we avoid finding the trivial 0 for in-place
        // animations.
        let mut i = 1u16;
        let mut y = self.oscillate(f64::from(i) / 1000.);

        while (self.to - self.from > f64::EPSILON && self.to - y > self.params.epsilon)
            || (self.from - self.to > f64::EPSILON && y - self.to > self.params.epsilon)
        {
            if i > 3000 {
                return None;
            }

            i += 1;
            y = self.oscillate(f64::from(i) / 1000.);
        }

        Some(Duration::from_millis(u64::from(i)))
    }

    /// Returns the spring position at a given time in seconds.
    fn oscillate(&self, t: f64) -> f64 {
        let b = self.params.damping;
        let m = self.params.mass;
        let k = self.params.stiffness;
        let v0 = self.initial_velocity;

        let beta = b / (2. * m);
        let omega0 = (k / m).sqrt();

        let x0 = self.from - self.to;

        let envelope = (-beta * t).exp();

        // Solutions of the form C1*e^(lambda1*x) + C2*e^(lambda2*x)
        // for the differential equation m*ẍ+b*ẋ+kx = 0

        // f64::EPSILON is too small for this specific comparison, so we use
        // f32::EPSILON even though it's doubles.
        if (beta - omega0).abs() <= f64::from(f32::EPSILON) {
            // Critically damped.
            self.to + envelope * (x0 + (beta * x0 + v0) * t)
        } else if beta < omega0 {
            // Underdamped.
            let omega1 = ((omega0 * omega0) - (beta * beta)).sqrt();

            self.to
                + envelope
                    * (x0 * (omega1 * t).cos() + ((beta * x0 + v0) / omega1) * (omega1 * t).sin())
        } else {
            // Overdamped.
            let omega2 = ((beta * beta) - (omega0 * omega0)).sqrt();

            self.to
                + envelope
                    * (x0 * (omega2 * t).cosh() + ((beta * x0 + v0) / omega2) * (omega2 * t).sinh())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overdamped_spring_equal_from_to_nan() {
        let spring = Spring {
            from: 0.,
            to: 0.,
            initial_velocity: 0.,
            params: SpringParams::new(1.15, 850., 0.0001),
        };
        let _ = spring.duration();
        let _ = spring.clamped_duration();
        let _ = spring.value_at(Duration::ZERO);
    }

    #[test]
    fn overdamped_spring_duration_panic() {
        let spring = Spring {
            from: 0.,
            to: 1.,
            initial_velocity: 0.,
            params: SpringParams::new(6., 1200., 0.0001),
        };
        let _ = spring.duration();
        let _ = spring.clamped_duration();
        let _ = spring.value_at(Duration::ZERO);
    }

    #[test]
    fn overdamped_solver_failure_does_not_collapse_duration() {
        let spring = Spring {
            from: 0.,
            to: 1.,
            initial_velocity: -184_171.825_578_530_55,
            params: SpringParams::new(9.370_388_960_701_666, 8381., 0.099_378_578_335_566_65),
        };

        let duration = spring.duration();
        assert!(duration >= Duration::from_millis(1));
        assert_ne!(duration, Duration::MAX);
    }

    #[test]
    fn overdamped_non_finite_newton_step_uses_finite_fallback() {
        let spring = Spring {
            from: 0.,
            to: 1.,
            initial_velocity: 10.,
            params: SpringParams::new(3., 1., 0.1),
        };

        let duration = spring.duration();
        assert!(duration >= Duration::from_millis(1));
        assert_ne!(duration, Duration::MAX);
    }
}
