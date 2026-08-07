//! Opt-in lifecycle and lifecycle-adjacent observation counters.
//!
//! Production keeps these quiet by default. When `NIRI_LIFECYCLE_DIAG=1` (or tests call
//! [`enable`]), counters accumulate without per-frame string allocation or logging.
//! Tracy spans remain the GPU/CPU span source of truth.
//!
//! Hot-path notes (`note_*`) check [`is_enabled`] first and return without further work when
//! disabled. Callers that need non-trivial work to build a sample must gate that work on
//! [`is_enabled`] or pass a lazy `FnOnce` (see [`note_genie_create`]).

use std::env;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

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

// P03 live-blur cache observation: how often the framebuffer effect actually
// re-blits the framebuffer, and how often a blur pyramid runs (live + xray).
static FB_EFFECT_CAPTURE: AtomicU64 = AtomicU64::new(0);
static BLUR_RENDER: AtomicU64 = AtomicU64::new(0);
static BLUR_TEXTURE_ALLOCATION_COUNT: AtomicU64 = AtomicU64::new(0);
static BLUR_TEXTURE_ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static BLUR_TEXTURE_REUSES: AtomicU64 = AtomicU64::new(0);
static BLUR_BUDGET_RESERVATION_FAILURES: AtomicU64 = AtomicU64::new(0);
static BLUR_FALLBACKS: AtomicU64 = AtomicU64::new(0);
static BLUR_GPU_ERRORS: AtomicU64 = AtomicU64::new(0);

// R17 redraw-attribution reason counters (cluster apply path only).
static REDRAW_TARGETED_LIFECYCLE: AtomicU64 = AtomicU64::new(0);
static REDRAW_TARGETED_ACTIVATE: AtomicU64 = AtomicU64::new(0);
static REDRAW_TARGETED_MAXIMIZE: AtomicU64 = AtomicU64::new(0);
static REDRAW_TARGETED_GLASS: AtomicU64 = AtomicU64::new(0);
static REDRAW_TARGETED_ACTION: AtomicU64 = AtomicU64::new(0);
static REDRAW_FALLBACK_UNLOCATABLE: AtomicU64 = AtomicU64::new(0);
static REDRAW_FALLBACK_OUTPUT_TEARDOWN: AtomicU64 = AtomicU64::new(0);
static REDRAW_FALLBACK_GLOBAL_CONFIG: AtomicU64 = AtomicU64::new(0);
static REDRAW_FALLBACK_GLOBAL_UI: AtomicU64 = AtomicU64::new(0);
static REDRAW_SKIP_UNMAPPED: AtomicU64 = AtomicU64::new(0);

fn ensure_init() {
    // Fast path: avoid the atomic RMW on every hot-path is_enabled() call.
    if INIT.load(Ordering::Relaxed) {
        return;
    }
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
    FB_EFFECT_CAPTURE.store(0, Ordering::Relaxed);
    BLUR_RENDER.store(0, Ordering::Relaxed);
    BLUR_TEXTURE_ALLOCATION_COUNT.store(0, Ordering::Relaxed);
    BLUR_TEXTURE_ALLOCATIONS.store(0, Ordering::Relaxed);
    BLUR_TEXTURE_REUSES.store(0, Ordering::Relaxed);
    BLUR_BUDGET_RESERVATION_FAILURES.store(0, Ordering::Relaxed);
    BLUR_FALLBACKS.store(0, Ordering::Relaxed);
    BLUR_GPU_ERRORS.store(0, Ordering::Relaxed);
    REDRAW_TARGETED_LIFECYCLE.store(0, Ordering::Relaxed);
    REDRAW_TARGETED_ACTIVATE.store(0, Ordering::Relaxed);
    REDRAW_TARGETED_MAXIMIZE.store(0, Ordering::Relaxed);
    REDRAW_TARGETED_GLASS.store(0, Ordering::Relaxed);
    REDRAW_TARGETED_ACTION.store(0, Ordering::Relaxed);
    REDRAW_FALLBACK_UNLOCATABLE.store(0, Ordering::Relaxed);
    REDRAW_FALLBACK_OUTPUT_TEARDOWN.store(0, Ordering::Relaxed);
    REDRAW_FALLBACK_GLOBAL_CONFIG.store(0, Ordering::Relaxed);
    REDRAW_FALLBACK_GLOBAL_UI.store(0, Ordering::Relaxed);
    REDRAW_SKIP_UNMAPPED.store(0, Ordering::Relaxed);
    THUMBNAIL_RENDER.store(0, Ordering::Relaxed);
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

/// Record a Genie snapshot creation.
///
/// `variant_bytes` is only evaluated when diagnostics are enabled, so production stays at a
/// single atomic enabled-check on the create path.
pub fn note_genie_create(variant_bytes: impl FnOnce() -> u64) {
    if !is_enabled() {
        return;
    }
    let variant_bytes = variant_bytes();
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

/// Record an actual framebuffer blit in `FramebufferEffectElement::capture_framebuffer`.
pub fn note_fb_effect_capture() {
    if !is_enabled() {
        return;
    }
    FB_EFFECT_CAPTURE.fetch_add(1, Ordering::Relaxed);
}

/// Record a full blur pyramid run in `Blur::render` (live and xray paths).
pub fn note_blur_render() {
    if !is_enabled() {
        return;
    }
    BLUR_RENDER.fetch_add(1, Ordering::Relaxed);
}

/// Record a blur pyramid texture allocation in bytes. This remains a counter
/// only; detailed attribution is opt-in through `NIRI_BLUR_TRACE`.
pub fn note_blur_texture_allocation(bytes: u64) {
    if !is_enabled() {
        return;
    }
    BLUR_TEXTURE_ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
    BLUR_TEXTURE_ALLOCATIONS.fetch_add(bytes, Ordering::Relaxed);
}

pub fn note_blur_texture_reuse() {
    if is_enabled() {
        BLUR_TEXTURE_REUSES.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn note_blur_budget_reservation_failure() {
    if is_enabled() {
        BLUR_BUDGET_RESERVATION_FAILURES.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn note_blur_fallback() {
    if is_enabled() {
        BLUR_FALLBACKS.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn note_blur_gpu_error() {
    if is_enabled() {
        BLUR_GPU_ERRORS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Targeted lifecycle redraw apply (one apply may queue one or more outputs).
pub fn note_redraw_targeted_lifecycle() {
    if is_enabled() {
        REDRAW_TARGETED_LIFECYCLE.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn note_redraw_targeted_activate() {
    if is_enabled() {
        REDRAW_TARGETED_ACTIVATE.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn note_redraw_targeted_maximize() {
    if is_enabled() {
        REDRAW_TARGETED_MAXIMIZE.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn note_redraw_targeted_glass() {
    if is_enabled() {
        REDRAW_TARGETED_GLASS.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn note_redraw_targeted_action() {
    if is_enabled() {
        REDRAW_TARGETED_ACTION.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn note_redraw_fallback_unlocatable() {
    if is_enabled() {
        REDRAW_FALLBACK_UNLOCATABLE.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn note_redraw_fallback_output_teardown() {
    if is_enabled() {
        REDRAW_FALLBACK_OUTPUT_TEARDOWN.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn note_redraw_fallback_global_config() {
    if is_enabled() {
        REDRAW_FALLBACK_GLOBAL_CONFIG.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn note_redraw_fallback_global_ui() {
    if is_enabled() {
        REDRAW_FALLBACK_GLOBAL_UI.fetch_add(1, Ordering::Relaxed);
    }
}

/// Record a glass redraw request that was skipped because the surface is not
/// rendered anywhere — unmapped, destroyed, or a layer whose output was
/// removed: no frame is needed, and no fallback redraw was queued (A03.3).
pub fn note_redraw_skip_unmapped() {
    if is_enabled() {
        REDRAW_SKIP_UNMAPPED.fetch_add(1, Ordering::Relaxed);
    }
}

// T05: how many times the thumbnail pipeline performed a real GPU capture
// (cache hits and rejected/skipped requests are not counted).
static THUMBNAIL_RENDER: AtomicU64 = AtomicU64::new(0);

/// Record a real thumbnail GPU capture (T05).
pub fn note_thumbnail_render() {
    if is_enabled() {
        THUMBNAIL_RENDER.fetch_add(1, Ordering::Relaxed);
    }
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
    pub fb_effect_capture: u64,
    pub blur_render: u64,
    pub blur_texture_allocations: u64,
    pub blur_texture_allocated_bytes: u64,
    pub blur_texture_reuses: u64,
    pub blur_budget_reservation_failures: u64,
    pub blur_fallbacks: u64,
    pub blur_gpu_errors: u64,
    pub redraw_targeted_lifecycle: u64,
    pub redraw_targeted_activate: u64,
    pub redraw_targeted_maximize: u64,
    pub redraw_targeted_glass: u64,
    pub redraw_fallback_unlocatable: u64,
    pub redraw_fallback_output_teardown: u64,
    pub redraw_fallback_global_config: u64,
    pub redraw_targeted_action: u64,
    pub redraw_fallback_global_ui: u64,
    pub redraw_skip_unmapped: u64,
    pub thumbnail_render: u64,
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
        fb_effect_capture: FB_EFFECT_CAPTURE.load(Ordering::Relaxed),
        blur_render: BLUR_RENDER.load(Ordering::Relaxed),
        blur_texture_allocations: BLUR_TEXTURE_ALLOCATION_COUNT.load(Ordering::Relaxed),
        blur_texture_allocated_bytes: BLUR_TEXTURE_ALLOCATIONS.load(Ordering::Relaxed),
        blur_texture_reuses: BLUR_TEXTURE_REUSES.load(Ordering::Relaxed),
        blur_budget_reservation_failures: BLUR_BUDGET_RESERVATION_FAILURES.load(Ordering::Relaxed),
        blur_fallbacks: BLUR_FALLBACKS.load(Ordering::Relaxed),
        blur_gpu_errors: BLUR_GPU_ERRORS.load(Ordering::Relaxed),
        redraw_targeted_lifecycle: REDRAW_TARGETED_LIFECYCLE.load(Ordering::Relaxed),
        redraw_targeted_activate: REDRAW_TARGETED_ACTIVATE.load(Ordering::Relaxed),
        redraw_targeted_maximize: REDRAW_TARGETED_MAXIMIZE.load(Ordering::Relaxed),
        redraw_targeted_glass: REDRAW_TARGETED_GLASS.load(Ordering::Relaxed),
        redraw_fallback_unlocatable: REDRAW_FALLBACK_UNLOCATABLE.load(Ordering::Relaxed),
        redraw_fallback_output_teardown: REDRAW_FALLBACK_OUTPUT_TEARDOWN.load(Ordering::Relaxed),
        redraw_fallback_global_config: REDRAW_FALLBACK_GLOBAL_CONFIG.load(Ordering::Relaxed),
        redraw_targeted_action: REDRAW_TARGETED_ACTION.load(Ordering::Relaxed),
        redraw_fallback_global_ui: REDRAW_FALLBACK_GLOBAL_UI.load(Ordering::Relaxed),
        redraw_skip_unmapped: REDRAW_SKIP_UNMAPPED.load(Ordering::Relaxed),
        thumbnail_render: THUMBNAIL_RENDER.load(Ordering::Relaxed),
    }
}

/// Serialize test access to the process-global enable flag and counters.
///
/// Production hot paths do not take this lock; only tests that flip enablement need isolation
/// from parallel `cargo test` threads.
#[cfg(test)]
pub fn with_test_lock<R>(f: impl FnOnce() -> R) -> R {
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f()
}

/// Log a per-interval counter delta line, at most once every 5 seconds.
///
/// Called from the redraw hot path; when diagnostics are disabled this costs
/// two relaxed atomic loads. The log line is the P03 acceptance signal: on a
/// static desktop `fb_capture` and `blur` must stay flat between intervals
/// while `redraw` may still advance (draw-only repaints reuse the cache).
pub fn maybe_log_periodic() {
    if !is_enabled() {
        return;
    }

    static LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);
    static LAST: Mutex<Option<Snapshot>> = Mutex::new(None);
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

    // Monotonic clock so a wall-clock jump can never suppress the log.
    let now_ms = START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64;
    let last_ms = LAST_LOG_MS.load(Ordering::Relaxed);
    if now_ms.saturating_sub(last_ms) < 5_000 && last_ms != 0 {
        return;
    }
    if LAST_LOG_MS
        .compare_exchange(last_ms, now_ms, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    let current = snapshot();
    let mut guard = LAST.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(prev) = *guard {
        info!(
            "lifecycle-diag 5s delta: redraw_all +{} redraw +{} tahoe_capture +{} \
             fb_capture +{} blur +{} blur_alloc +{} ({} bytes) blur_reuse +{} \
             blur_budget_fail +{} blur_fallback +{} blur_gpu_error +{}",
            current
                .queue_redraw_all
                .saturating_sub(prev.queue_redraw_all),
            current.queue_redraw.saturating_sub(prev.queue_redraw),
            current
                .tahoe_region_capture
                .saturating_sub(prev.tahoe_region_capture),
            current
                .fb_effect_capture
                .saturating_sub(prev.fb_effect_capture),
            current.blur_render.saturating_sub(prev.blur_render),
            current
                .blur_texture_allocations
                .saturating_sub(prev.blur_texture_allocations),
            current
                .blur_texture_allocated_bytes
                .saturating_sub(prev.blur_texture_allocated_bytes),
            current
                .blur_texture_reuses
                .saturating_sub(prev.blur_texture_reuses),
            current
                .blur_budget_reservation_failures
                .saturating_sub(prev.blur_budget_reservation_failures),
            current.blur_fallbacks.saturating_sub(prev.blur_fallbacks),
            current.blur_gpu_errors.saturating_sub(prev.blur_gpu_errors),
        );
    }
    *guard = Some(current);
}

/// Enable, reset, run `f`, then always disable+reset — under [`with_test_lock`].
#[cfg(test)]
pub fn with_enabled_for_test<R>(f: impl FnOnce() -> R) -> R {
    with_test_lock(|| {
        enable();
        reset();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        disable_and_reset();
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_by_default_has_zero_cost_path() {
        with_test_lock(|| {
            disable_and_reset();
            assert!(!is_enabled());
            note_queue_redraw_all();
            note_genie_create(|| 1024);
            assert_eq!(snapshot(), Snapshot::default());
        });
    }

    #[test]
    fn enabled_counters_accumulate_without_logging() {
        with_enabled_for_test(|| {
            note_queue_redraw_all();
            note_queue_redraw();
            note_genie_create(|| 100);
            note_genie_create(|| 250);
            note_tahoe_region_request();
            note_tahoe_region_commit();
            note_tahoe_region_capture();
            note_blur_render();
            note_blur_texture_allocation(4096);
            note_blur_texture_reuse();
            note_blur_budget_reservation_failure();
            note_blur_fallback();
            note_blur_gpu_error();
            note_redraw_skip_unmapped();

            let s = snapshot();
            assert_eq!(s.queue_redraw_all, 1);
            assert_eq!(s.queue_redraw, 1);
            assert_eq!(s.genie_create, 2);
            assert_eq!(s.snapshot_variant_bytes, 350);
            assert_eq!(s.snapshot_peak_bytes, 250);
            assert_eq!(s.tahoe_region_request, 1);
            assert_eq!(s.tahoe_region_commit, 1);
            assert_eq!(s.tahoe_region_capture, 1);
            assert_eq!(s.blur_render, 1);
            assert_eq!(s.blur_texture_allocations, 1);
            assert_eq!(s.blur_texture_allocated_bytes, 4096);
            assert_eq!(s.blur_texture_reuses, 1);
            assert_eq!(s.blur_budget_reservation_failures, 1);
            assert_eq!(s.blur_fallbacks, 1);
            assert_eq!(s.blur_gpu_errors, 1);
            assert_eq!(s.redraw_skip_unmapped, 1);
        });
    }

    #[test]
    fn note_genie_create_lazy_closure_not_run_when_disabled() {
        with_test_lock(|| {
            disable_and_reset();
            let mut ran = false;
            note_genie_create(|| {
                ran = true;
                99
            });
            assert!(!ran, "disabled path must not evaluate variant_bytes");
            assert_eq!(snapshot().genie_create, 0);
        });
    }
}
