use std::env;
use std::num::NonZeroU64;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::Element;
use smithay::output::Output;

use crate::utils::get_monotonic_time;

const FRAME_TELEMETRY_ENV: &str = "NIRI_FRAME_TELEMETRY";
const FRAME_TELEMETRY_REPORT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RedrawSources {
    pub layout: bool,
    pub cursor: bool,
    pub layer: bool,
    pub config_error_ui: bool,
    pub exit_confirm_ui: bool,
    pub screenshot_ui: bool,
    pub window_mru_ui: bool,
    pub screen_transition: bool,
    pub closing_layer: bool,
}

impl RedrawSources {
    pub fn any(self) -> bool {
        self.layout
            || self.cursor
            || self.layer
            || self.config_error_ui
            || self.exit_confirm_ui
            || self.screenshot_ui
            || self.window_mru_ui
            || self.screen_transition
            || self.closing_layer
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOutcome {
    Submitted,
    NoDamage,
    Skipped,
}

#[derive(Debug, Default)]
struct TelemetryWindow {
    redraws: u64,
    redraws_without_ongoing: u64,
    submitted: u64,
    no_damage: u64,
    skipped: u64,
    presented: u64,
    direct_scanout_frames: u64,
    composited_frames: u64,
    source_layout: u64,
    source_cursor: u64,
    source_layer: u64,
    source_ui: u64,
    source_config_error_ui: u64,
    source_exit_confirm_ui: u64,
    source_screenshot_ui: u64,
    source_window_mru_ui: u64,
    source_screen_transition: u64,
    source_closing_layer: u64,
    render_times_ns: Vec<u64>,
    frame_times_ns: Vec<u64>,
    damage_pixels: Vec<u64>,
}

impl TelemetryWindow {
    fn record_redraw(
        &mut self,
        sources: RedrawSources,
        outcome: FrameOutcome,
        render_time: Duration,
    ) {
        self.redraws += 1;
        self.redraws_without_ongoing += u64::from(!sources.any());
        self.submitted += u64::from(outcome == FrameOutcome::Submitted);
        self.no_damage += u64::from(outcome == FrameOutcome::NoDamage);
        self.skipped += u64::from(outcome == FrameOutcome::Skipped);
        self.source_layout += u64::from(sources.layout);
        self.source_cursor += u64::from(sources.cursor);
        self.source_layer += u64::from(sources.layer);
        self.source_ui += u64::from(
            sources.config_error_ui
                || sources.exit_confirm_ui
                || sources.screenshot_ui
                || sources.window_mru_ui,
        );
        self.source_config_error_ui += u64::from(sources.config_error_ui);
        self.source_exit_confirm_ui += u64::from(sources.exit_confirm_ui);
        self.source_screenshot_ui += u64::from(sources.screenshot_ui);
        self.source_window_mru_ui += u64::from(sources.window_mru_ui);
        self.source_screen_transition += u64::from(sources.screen_transition);
        self.source_closing_layer += u64::from(sources.closing_layer);
        self.render_times_ns.push(duration_ns(render_time));
    }

    fn record_presented(&mut self, frame_time: Option<Duration>) {
        self.presented += 1;
        if let Some(frame_time) = frame_time {
            self.frame_times_ns.push(duration_ns(frame_time));
        }
    }

    fn record_direct_scanout(&mut self, direct_scanout: bool) {
        if direct_scanout {
            self.direct_scanout_frames += 1;
        } else {
            self.composited_frames += 1;
        }
    }

    fn report(&mut self, elapsed: Duration, output_pixels: u64) -> TelemetryReport {
        self.render_times_ns.sort_unstable();
        self.frame_times_ns.sort_unstable();
        self.damage_pixels.sort_unstable();

        let elapsed_s = elapsed.as_secs_f64().max(f64::EPSILON);
        let damage_sum = self
            .damage_pixels
            .iter()
            .fold(0_u128, |sum, value| sum + u128::from(*value));
        let damage_avg_pixels = if self.damage_pixels.is_empty() {
            0.
        } else {
            damage_sum as f64 / self.damage_pixels.len() as f64
        };
        let damage_avg_percent = if output_pixels == 0 {
            0.
        } else {
            damage_avg_pixels * 100. / output_pixels as f64
        };
        let scanout_samples = self.direct_scanout_frames + self.composited_frames;
        let direct_scanout_percent = if scanout_samples == 0 {
            0.
        } else {
            self.direct_scanout_frames as f64 * 100. / scanout_samples as f64
        };

        TelemetryReport {
            elapsed_s,
            redraws: self.redraws,
            redraws_without_ongoing: self.redraws_without_ongoing,
            submitted_fps: self.submitted as f64 / elapsed_s,
            presented_fps: self.presented as f64 / elapsed_s,
            submitted: self.submitted,
            no_damage: self.no_damage,
            skipped: self.skipped,
            direct_scanout_frames: self.direct_scanout_frames,
            composited_frames: self.composited_frames,
            direct_scanout_percent,
            render_p95_ms: ns_to_ms(percentile(&self.render_times_ns, 95)),
            render_p99_ms: ns_to_ms(percentile(&self.render_times_ns, 99)),
            frame_p50_ms: ns_to_ms(percentile(&self.frame_times_ns, 50)),
            frame_p95_ms: ns_to_ms(percentile(&self.frame_times_ns, 95)),
            frame_p99_ms: ns_to_ms(percentile(&self.frame_times_ns, 99)),
            damage_avg_pixels,
            damage_p95_pixels: percentile(&self.damage_pixels, 95),
            damage_p99_pixels: percentile(&self.damage_pixels, 99),
            damage_avg_percent,
            source_layout: self.source_layout,
            source_cursor: self.source_cursor,
            source_layer: self.source_layer,
            source_ui: self.source_ui,
            source_config_error_ui: self.source_config_error_ui,
            source_exit_confirm_ui: self.source_exit_confirm_ui,
            source_screenshot_ui: self.source_screenshot_ui,
            source_window_mru_ui: self.source_window_mru_ui,
            source_screen_transition: self.source_screen_transition,
            source_closing_layer: self.source_closing_layer,
        }
    }

    fn reset(&mut self) {
        self.redraws = 0;
        self.redraws_without_ongoing = 0;
        self.submitted = 0;
        self.no_damage = 0;
        self.skipped = 0;
        self.presented = 0;
        self.direct_scanout_frames = 0;
        self.composited_frames = 0;
        self.source_layout = 0;
        self.source_cursor = 0;
        self.source_layer = 0;
        self.source_ui = 0;
        self.source_config_error_ui = 0;
        self.source_exit_confirm_ui = 0;
        self.source_screenshot_ui = 0;
        self.source_window_mru_ui = 0;
        self.source_screen_transition = 0;
        self.source_closing_layer = 0;
        self.render_times_ns.clear();
        self.frame_times_ns.clear();
        self.damage_pixels.clear();
    }
}

#[derive(Debug)]
struct TelemetryReport {
    elapsed_s: f64,
    redraws: u64,
    redraws_without_ongoing: u64,
    submitted_fps: f64,
    presented_fps: f64,
    submitted: u64,
    no_damage: u64,
    skipped: u64,
    direct_scanout_frames: u64,
    composited_frames: u64,
    direct_scanout_percent: f64,
    render_p95_ms: f64,
    render_p99_ms: f64,
    frame_p50_ms: f64,
    frame_p95_ms: f64,
    frame_p99_ms: f64,
    damage_avg_pixels: f64,
    damage_p95_pixels: u64,
    damage_p99_pixels: u64,
    damage_avg_percent: f64,
    source_layout: u64,
    source_cursor: u64,
    source_layer: u64,
    source_ui: u64,
    source_config_error_ui: u64,
    source_exit_confirm_ui: u64,
    source_screenshot_ui: u64,
    source_window_mru_ui: u64,
    source_screen_transition: u64,
    source_closing_layer: u64,
}

pub struct FrameTelemetry {
    output_name: String,
    damage_tracker: OutputDamageTracker,
    window_started: Instant,
    last_presentation_time: Option<Duration>,
    output_pixels: u64,
    window: TelemetryWindow,
}

impl FrameTelemetry {
    pub fn for_output(output: &Output) -> Option<Self> {
        if !frame_telemetry_enabled() {
            return None;
        }

        info!(
            target: "niri::frame_telemetry",
            output = output.name(),
            report_interval_s = FRAME_TELEMETRY_REPORT_INTERVAL.as_secs(),
            "frame telemetry enabled"
        );

        Some(Self {
            output_name: output.name(),
            damage_tracker: OutputDamageTracker::from_output(output),
            window_started: Instant::now(),
            last_presentation_time: None,
            output_pixels: output_pixel_count(output),
            window: TelemetryWindow::default(),
        })
    }

    pub fn record_damage<E: Element>(&mut self, output: &Output, elements: &[E]) {
        self.output_pixels = output_pixel_count(output);
        let Ok((damage, _)) = self.damage_tracker.damage_output(1, elements) else {
            return;
        };

        let pixels = damage
            .into_iter()
            .flatten()
            .map(|rect| {
                let width = u64::try_from(rect.size.w).unwrap_or(0);
                let height = u64::try_from(rect.size.h).unwrap_or(0);
                width.saturating_mul(height)
            })
            .sum();
        self.window.damage_pixels.push(pixels);
    }

    pub fn record_presented(&mut self, presentation_time: Duration) {
        let frame_time = self
            .last_presentation_time
            .and_then(|last| presentation_time.checked_sub(last));
        self.last_presentation_time = Some(presentation_time);
        self.window.record_presented(frame_time);
    }

    pub fn record_direct_scanout(&mut self, direct_scanout: bool) {
        self.window.record_direct_scanout(direct_scanout);
    }

    pub fn record_redraw(
        &mut self,
        sources: RedrawSources,
        outcome: FrameOutcome,
        render_time: Duration,
    ) {
        self.window.record_redraw(sources, outcome, render_time);

        let elapsed = self.window_started.elapsed();
        if elapsed < FRAME_TELEMETRY_REPORT_INTERVAL {
            return;
        }

        let report = self.window.report(elapsed, self.output_pixels);
        info!(
            target: "niri::frame_telemetry",
            output = self.output_name,
            window_s = report.elapsed_s,
            redraws = report.redraws,
            redraws_without_ongoing = report.redraws_without_ongoing,
            submitted = report.submitted,
            no_damage = report.no_damage,
            skipped = report.skipped,
            direct_scanout_frames = report.direct_scanout_frames,
            composited_frames = report.composited_frames,
            direct_scanout_percent = report.direct_scanout_percent,
            submitted_fps = report.submitted_fps,
            presented_fps = report.presented_fps,
            render_p95_ms = report.render_p95_ms,
            render_p99_ms = report.render_p99_ms,
            frame_time_p50_ms = report.frame_p50_ms,
            frame_time_p95_ms = report.frame_p95_ms,
            frame_time_p99_ms = report.frame_p99_ms,
            damage_avg_pixels = report.damage_avg_pixels,
            damage_p95_pixels = report.damage_p95_pixels,
            damage_p99_pixels = report.damage_p99_pixels,
            damage_avg_percent = report.damage_avg_percent,
            source_layout = report.source_layout,
            source_cursor = report.source_cursor,
            source_layer = report.source_layer,
            source_ui = report.source_ui,
            source_config_error_ui = report.source_config_error_ui,
            source_exit_confirm_ui = report.source_exit_confirm_ui,
            source_screenshot_ui = report.source_screenshot_ui,
            source_window_mru_ui = report.source_window_mru_ui,
            source_screen_transition = report.source_screen_transition,
            source_closing_layer = report.source_closing_layer,
            "frame telemetry"
        );

        self.window.reset();
        self.window_started = Instant::now();
    }
}

fn frame_telemetry_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();

    *ENABLED.get_or_init(|| {
        env::var_os(FRAME_TELEMETRY_ENV).is_some_and(|value| {
            !matches!(
                value.to_string_lossy().to_ascii_lowercase().as_str(),
                "" | "0" | "false" | "no" | "off"
            )
        })
    })
}

fn output_pixel_count(output: &Output) -> u64 {
    output.current_mode().map_or(0, |mode| {
        let width = u64::try_from(mode.size.w).unwrap_or(0);
        let height = u64::try_from(mode.size.h).unwrap_or(0);
        width.saturating_mul(height)
    })
}

fn duration_ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

fn percentile(sorted: &[u64], percent: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }

    let rank = sorted.len().saturating_mul(percent).div_ceil(100);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn ns_to_ms(ns: u64) -> f64 {
    ns as f64 / 1_000_000.
}

#[derive(Debug)]
pub struct FrameClock {
    last_presentation_time: Option<Duration>,
    refresh_interval_ns: Option<NonZeroU64>,
    vrr: bool,
}

impl FrameClock {
    pub fn new(refresh_interval: Option<Duration>, vrr: bool) -> Self {
        let refresh_interval_ns = if let Some(interval) = &refresh_interval {
            assert_eq!(interval.as_secs(), 0);
            Some(NonZeroU64::new(interval.subsec_nanos().into()).unwrap())
        } else {
            None
        };

        Self {
            last_presentation_time: None,
            refresh_interval_ns,
            vrr,
        }
    }

    pub fn refresh_interval(&self) -> Option<Duration> {
        self.refresh_interval_ns
            .map(|r| Duration::from_nanos(r.get()))
    }

    pub fn set_vrr(&mut self, vrr: bool) {
        if self.vrr == vrr {
            return;
        }

        self.vrr = vrr;
        self.last_presentation_time = None;
    }

    pub fn vrr(&self) -> bool {
        self.vrr
    }

    pub fn presented(&mut self, presentation_time: Duration) {
        if presentation_time.is_zero() {
            // Not interested in these.
            return;
        }

        self.last_presentation_time = Some(presentation_time);
    }

    pub fn next_presentation_time(&self) -> Duration {
        let mut now = get_monotonic_time();

        let Some(refresh_interval_ns) = self.refresh_interval_ns else {
            return now;
        };
        let Some(last_presentation_time) = self.last_presentation_time else {
            return now;
        };

        let refresh_interval_ns = refresh_interval_ns.get();

        if now <= last_presentation_time {
            // Got an early VBlank.
            let orig_now = now;
            now += Duration::from_nanos(refresh_interval_ns);

            if now < last_presentation_time {
                // Not sure when this can happen.
                error!(
                    now = ?orig_now,
                    ?last_presentation_time,
                    "got a 2+ early VBlank, {:?} until presentation",
                    last_presentation_time - now,
                );
                now = last_presentation_time + Duration::from_nanos(refresh_interval_ns);
            }
        }

        let since_last = now - last_presentation_time;
        let since_last_ns =
            since_last.as_secs() * 1_000_000_000 + u64::from(since_last.subsec_nanos());
        let to_next_ns = (since_last_ns / refresh_interval_ns + 1) * refresh_interval_ns;

        // If VRR is enabled and more than one frame passed since last presentation, assume that we
        // can present immediately.
        if self.vrr && to_next_ns > refresh_interval_ns {
            now
        } else {
            last_presentation_time + Duration::from_nanos(to_next_ns)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_uses_nearest_rank() {
        let values = (1..=100).collect::<Vec<_>>();
        assert_eq!(percentile(&values, 95), 95);
        assert_eq!(percentile(&values, 99), 99);
        assert_eq!(percentile(&[], 99), 0);
    }

    #[test]
    fn telemetry_counts_sources_and_percentiles_independently() {
        let mut window = TelemetryWindow::default();
        window.damage_pixels.extend([10, 20, 30, 40]);
        window.record_presented(None);
        window.record_presented(Some(Duration::from_millis(5)));
        window.record_direct_scanout(true);
        window.record_direct_scanout(false);
        window.record_redraw(
            RedrawSources {
                layout: true,
                screenshot_ui: true,
                ..Default::default()
            },
            FrameOutcome::Submitted,
            Duration::from_millis(2),
        );
        window.record_redraw(
            RedrawSources {
                cursor: true,
                layer: true,
                screen_transition: true,
                closing_layer: true,
                ..Default::default()
            },
            FrameOutcome::NoDamage,
            Duration::from_millis(4),
        );
        window.record_redraw(
            RedrawSources {
                config_error_ui: true,
                exit_confirm_ui: true,
                window_mru_ui: true,
                ..Default::default()
            },
            FrameOutcome::Skipped,
            Duration::from_millis(3),
        );

        let report = window.report(Duration::from_secs(2), 100);
        assert_eq!(report.redraws, 3);
        assert_eq!(report.submitted, 1);
        assert_eq!(report.no_damage, 1);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.direct_scanout_frames, 1);
        assert_eq!(report.composited_frames, 1);
        assert_eq!(report.direct_scanout_percent, 50.);
        assert_eq!(report.source_layout, 1);
        assert_eq!(report.source_cursor, 1);
        assert_eq!(report.source_layer, 1);
        assert_eq!(report.source_ui, 2);
        assert_eq!(report.source_config_error_ui, 1);
        assert_eq!(report.source_exit_confirm_ui, 1);
        assert_eq!(report.source_screenshot_ui, 1);
        assert_eq!(report.source_window_mru_ui, 1);
        assert_eq!(report.source_screen_transition, 1);
        assert_eq!(report.source_closing_layer, 1);
        assert_eq!(report.render_p95_ms, 4.);
        assert_eq!(report.frame_p50_ms, 5.);
        assert_eq!(report.frame_p95_ms, 5.);
        assert_eq!(report.damage_p95_pixels, 40);
        assert_eq!(report.damage_avg_percent, 25.);
        assert_eq!(report.submitted_fps, 0.5);
        assert_eq!(report.presented_fps, 1.);
    }
}
