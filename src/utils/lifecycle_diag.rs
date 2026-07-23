//! Opt-in lifecycle and lifecycle-adjacent observation counters.
//!
//! Production keeps these quiet by default. When `NIRI_LIFECYCLE_DIAG=1` (or tests call
//! [`enable`]), counters accumulate without per-frame string allocation or logging.
//! Tracy spans remain the GPU/CPU span source of truth.

use std::env;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const ENV: &str = "NIRI_LIFECYCLE_DIAG";

static ENABLED: AtomicBool = AtomicBool::new(false);
static INIT: AtomicBool = AtomicBool::new(false);

static QUEUE_REDRAW_ALL: AtomicU64 = AtomicU64::new(0);
static QUEUE_REDRAW_ONE: AtomicU64 = AtomicU64::new(0);
static GENIE_CREATE: AtomicU64 = AtomicU64::new(0);
static SNAPSHOT_VARIANT_BYTES: AtomicU64 = AtomicU64::new(0);
static SNAPSHOT_PEAK_BYTES: AtomicU64 = AtomicU64::new(0);
static TAHOE_REGION_REQUEST: AtomicU64 = AtomicU64::new(0);
static TAHOE_REGION_COMMIT: AtomicU64 = AtomicU64::new(0);
static TAHOE_REGION_CAPTURE: AtomicU64 = AtomicU64::new(0);

fn ensure_init() {
    if INIT.swap(true, Ordering::Relaxed) {
        return;
    }
    if env::var_os(ENV).is_some_and(|v| v != "0" && !v.is_empty()) {
        ENABLED.store(true, Ordering::Relaxed);
    }
}

/// Explicitly enable counters (tests). No-op when already enabled via env.
pub fn enable() {
    ensure_init();
    ENABLED.store(true, Ordering::Relaxed);
}

/// Disable counters and clear samples. Tests use this between cases.
pub fn disable_and_reset() {
    ENABLED.store(false, Ordering::Relaxed);
    reset();
}

pub fn is_enabled() -> bool {
    ensure_init();
    ENABLED.load(Ordering::Relaxed)
}

pub fn reset() {
    QUEUE_REDRAW_ALL.store(0, Ordering::Relaxed);
    QUEUE_REDRAW_ONE.store(0, Ordering::Relaxed);
    GENIE_CREATE.store(0, Ordering::Relaxed);
    SNAPSHOT_VARIANT_BYTES.store(0, Ordering::Relaxed);
    SNAPSHOT_PEAK_BYTES.store(0, Ordering::Relaxed);
    TAHOE_REGION_REQUEST.store(0, Ordering::Relaxed);
    TAHOE_REGION_COMMIT.store(0, Ordering::Relaxed);
    TAHOE_REGION_CAPTURE.store(0, Ordering::Relaxed);
}

pub fn note_queue_redraw_all() {
    if !is_enabled() {
        return;
    }
    QUEUE_REDRAW_ALL.fetch_add(1, Ordering::Relaxed);
}

pub fn note_queue_redraw() {
    if !is_enabled() {
        return;
    }
    QUEUE_REDRAW_ONE.fetch_add(1, Ordering::Relaxed);
}

pub fn note_genie_create(variant_bytes: u64) {
    if !is_enabled() {
        return;
    }
    GENIE_CREATE.fetch_add(1, Ordering::Relaxed);
    SNAPSHOT_VARIANT_BYTES.fetch_add(variant_bytes, Ordering::Relaxed);
    SNAPSHOT_PEAK_BYTES.fetch_max(variant_bytes, Ordering::Relaxed);
}

pub fn note_tahoe_region_request() {
    if !is_enabled() {
        return;
    }
    TAHOE_REGION_REQUEST.fetch_add(1, Ordering::Relaxed);
}

pub fn note_tahoe_region_commit() {
    if !is_enabled() {
        return;
    }
    TAHOE_REGION_COMMIT.fetch_add(1, Ordering::Relaxed);
}

pub fn note_tahoe_region_capture() {
    if !is_enabled() {
        return;
    }
    TAHOE_REGION_CAPTURE.fetch_add(1, Ordering::Relaxed);
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub queue_redraw_all: u64,
    pub queue_redraw: u64,
    pub genie_create: u64,
    pub snapshot_variant_bytes: u64,
    pub snapshot_peak_bytes: u64,
    pub tahoe_region_request: u64,
    pub tahoe_region_commit: u64,
    pub tahoe_region_capture: u64,
}

pub fn snapshot() -> Snapshot {
    Snapshot {
        queue_redraw_all: QUEUE_REDRAW_ALL.load(Ordering::Relaxed),
        queue_redraw: QUEUE_REDRAW_ONE.load(Ordering::Relaxed),
        genie_create: GENIE_CREATE.load(Ordering::Relaxed),
        snapshot_variant_bytes: SNAPSHOT_VARIANT_BYTES.load(Ordering::Relaxed),
        snapshot_peak_bytes: SNAPSHOT_PEAK_BYTES.load(Ordering::Relaxed),
        tahoe_region_request: TAHOE_REGION_REQUEST.load(Ordering::Relaxed),
        tahoe_region_commit: TAHOE_REGION_COMMIT.load(Ordering::Relaxed),
        tahoe_region_capture: TAHOE_REGION_CAPTURE.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_by_default_has_zero_cost_path() {
        disable_and_reset();
        assert!(!is_enabled());
        note_queue_redraw_all();
        note_genie_create(1024);
        assert_eq!(snapshot(), Snapshot::default());
    }

    #[test]
    fn enabled_counters_accumulate_without_logging() {
        enable();
        reset();
        note_queue_redraw_all();
        note_queue_redraw();
        note_genie_create(100);
        note_genie_create(250);
        note_tahoe_region_request();
        note_tahoe_region_commit();
        note_tahoe_region_capture();

        let s = snapshot();
        assert_eq!(s.queue_redraw_all, 1);
        assert_eq!(s.queue_redraw, 1);
        assert_eq!(s.genie_create, 2);
        assert_eq!(s.snapshot_variant_bytes, 350);
        assert_eq!(s.snapshot_peak_bytes, 250);
        assert_eq!(s.tahoe_region_request, 1);
        assert_eq!(s.tahoe_region_commit, 1);
        assert_eq!(s.tahoe_region_capture, 1);

        disable_and_reset();
    }
}
