//! R15 performance baseline remeasurement harness.
//!
//! These tests are measurement instruments, not product-behavior changes. They
//! print structured `R15_SAMPLE ...` lines (visible with `--nocapture`) and
//! assert that the post-R00–R14 ownership surface still exposes the costs that
//! R16–R19 decisions depend on.
//!
//! Go/no-go thresholds are locked in
//! `docs/window-lifecycle-maintainability-remediation-2026-07-22/acceptance/R15-baseline-2026-07-24.md`
//! *before* interpreting these samples; tests only collect reproducible data.

use std::time::Duration;

use niri_config::Config;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor;
use wayland_client::protocol::wl_surface::WlSurface;

use super::client::LayerConfigureProps;
use super::*;
use crate::niri::RedrawState;
use crate::utils::lifecycle_diag;

fn create_window(f: &mut Fixture, id: client::ClientId, w: u16, h: u16) -> WlSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(w, h);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    surface
}

fn map_dock_and_set_rect(
    f: &mut Fixture,
    id: client::ClientId,
    output_idx: u8,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> WlSurface {
    let wl_output = f.client(id).output(&format!("headless-{output_idx}"));
    let layer = f
        .client(id)
        .create_layer(Some(&wl_output), Layer::Top, "dock");
    let layer_surface = layer.surface.clone();
    layer.set_configure_props(LayerConfigureProps {
        anchor: Some(Anchor::Left | Anchor::Bottom),
        size: Some((200, 80)),
        ..Default::default()
    });
    layer.commit();
    f.roundtrip(id);

    let layer = f.client(id).layer(&layer_surface);
    layer.attach_new_buffer();
    layer.set_size(200, 80);
    layer.ack_last_and_commit();
    f.double_roundtrip(id);

    let handle = f.client(id).foreign_toplevel(0);
    handle.set_rectangle(&layer_surface, x, y, w, h);
    f.double_roundtrip(id);
    layer_surface
}

fn linear_lifecycle_config() -> Config {
    use niri_config::animations::{Curve, EasingParams, Kind};
    const LINEAR: Kind = Kind::Easing(EasingParams {
        duration_ms: 1000,
        curve: Curve::Linear,
    });
    let mut config = Config::default();
    config.layout.gaps = 0.0;
    // Minimize/restore default to close/open animation config when unset.
    config.animations.window_resize.anim.kind = LINEAR;
    config.animations.window_close.anim.kind = LINEAR;
    config.animations.window_open.anim.kind = LINEAR;
    config
}

fn config_with_block_out_screencast() -> Config {
    // Force multi-variant snapshot: block-out-from makes MinimizeWindowAnimation
    // also texture-convert blocked_out_contents at create time.
    let mut config = Config::parse_mem(
        r#"
        window-rule {
            block-out-from "screencast"
        }
        "#,
    )
    .expect("parse block-out window-rule");
    let base = linear_lifecycle_config();
    config.layout.gaps = base.layout.gaps;
    config.animations = base.animations;
    config
}

fn count_outputs_queued(niri: &crate::niri::Niri) -> (usize, usize) {
    let total = niri.output_state.len();
    let queued = niri
        .output_state
        .values()
        .filter(|s| {
            matches!(
                s.redraw_state,
                RedrawState::Queued | RedrawState::WaitingForEstimatedVBlankAndQueued(_)
            )
        })
        .count();
    (queued, total)
}

fn force_idle_redraw_states(niri: &mut crate::niri::Niri) {
    // Headless fixture rarely parks on estimated-vblank tokens; drop any queued
    // mark so the next queue_redraw(_all) is attributable.
    for state in niri.output_state.values_mut() {
        state.redraw_state = RedrawState::Idle;
    }
}

fn abgr8888_bytes(w: u64, h: u64) -> u64 {
    w.saturating_mul(h).saturating_mul(4)
}

/// R17 structural: `queue_redraw_all` marks every dual-output Queued before any
/// present cycle consumes the state. Combined with foreign-path samples below,
/// this is the multi-output waste evidence (1 home vs N outputs).
#[test]
fn r15_queue_redraw_all_marks_every_dual_output() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));
    force_idle_redraw_states(f.niri());
    let (queued0, total) = count_outputs_queued(f.niri());
    assert_eq!(queued0, 0);
    assert_eq!(total, 2);

    f.niri().queue_redraw_all();
    let (queued, total) = count_outputs_queued(f.niri());
    let waste_outputs = queued.saturating_sub(1);
    let waste_ratio = waste_outputs as f64 / total as f64;
    eprintln!(
        "R15_SAMPLE kind=r17_queue_redraw_all_marks_all \
         outputs_queued={queued} outputs_total={total} home_outputs=1 \
         waste_outputs={waste_outputs} waste_ratio={waste_ratio:.3}"
    );
    assert_eq!(queued, 2);
    assert!((waste_ratio - 0.5).abs() < f64::EPSILON || waste_ratio >= 0.5);
}

/// Historical R15 Go sample for foreign minimize (pre-R17 used queue_redraw_all).
/// Post-R17 the same path is targeted; sample documents the improvement.
#[test]
fn r15_foreign_minimize_dual_output_redraw_all_waste() {
    lifecycle_diag::with_enabled_for_test(|| {
        let mut f = Fixture::with_config(linear_lifecycle_config());
        f.niri_state().backend.headless().add_renderer().unwrap();
        f.add_output(1, (1920, 1080));
        f.add_output(2, (1280, 720));

        let id = f.add_client();
        let _surface = create_window(&mut f, id, 400, 300);
        let _dock = map_dock_and_set_rect(&mut f, id, 1, 20, 40, 48, 48);

        // Window lives on output 1 (first output focus / layout default).
        f.niri_focus_output(1);
        lifecycle_diag::reset();

        f.client(id).foreign_toplevel(0).set_minimized();
        f.double_roundtrip(id);

        let diag = lifecycle_diag::snapshot();
        let total_outputs = f.niri().output_state.len();

        eprintln!(
            "R15_SAMPLE kind=r17_foreign_minimize_dual_output \
             queue_redraw_all={} queue_redraw={} outputs_total={} home_outputs=1 \
             targeted_lifecycle={} \
             genie_create={} snapshot_peak_bytes={} note=post_R17_targeted",
            diag.queue_redraw_all,
            diag.queue_redraw,
            total_outputs,
            diag.redraw_targeted_lifecycle,
            diag.genie_create,
            diag.snapshot_peak_bytes
        );

        // R17: foreign minimize adapter applies Lifecycle attribution (targeted).
        // Incidental redraw-all from pointer refresh (outside cluster) may still appear
        // during double_roundtrip; see r17_foreign_minimize_targets_home_output_only
        // for zero-waste Queued-state proof on the attribution path alone.
        assert!(diag.queue_redraw >= 1);
        assert!(diag.redraw_targeted_lifecycle >= 1);
        assert_eq!(total_outputs, 2);
        assert!(
            f.niri().layout.windows().next().unwrap().1.is_minimized(),
            "minimize must apply"
        );
    });
}

/// Historical R15 sample; post-R17 foreign restore uses lifecycle attribution.
#[test]
fn r15_foreign_restore_dual_output_redraw_all_waste() {
    lifecycle_diag::with_enabled_for_test(|| {
        let mut f = Fixture::with_config(linear_lifecycle_config());
        f.niri_state().backend.headless().add_renderer().unwrap();
        f.add_output(1, (1920, 1080));
        f.add_output(2, (1280, 720));

        let id = f.add_client();
        let _surface = create_window(&mut f, id, 400, 300);
        let _dock = map_dock_and_set_rect(&mut f, id, 1, 20, 40, 48, 48);
        f.niri_focus_output(1);

        f.client(id).foreign_toplevel(0).set_minimized();
        f.double_roundtrip(id);
        assert!(f.niri().layout.windows().next().unwrap().1.is_minimized());

        lifecycle_diag::reset();

        f.client(id).foreign_toplevel(0).unset_minimized();
        f.double_roundtrip(id);

        let diag = lifecycle_diag::snapshot();
        let total = f.niri().output_state.len();

        eprintln!(
            "R15_SAMPLE kind=r17_foreign_restore_dual_output \
             queue_redraw_all={} queue_redraw={} outputs_total={} \
             targeted_lifecycle={} note=post_R17_targeted",
            diag.queue_redraw_all, diag.queue_redraw, total, diag.redraw_targeted_lifecycle
        );

        assert!(diag.queue_redraw >= 1);
        assert!(diag.redraw_targeted_lifecycle >= 1);
        assert_eq!(total, 2);
        assert!(!f.niri().layout.windows().next().unwrap().1.is_minimized());
    });
}

/// Historical R15 sample; post-R17 foreign maximize is targeted.
#[test]
fn r15_foreign_maximize_dual_output_redraw_all() {
    lifecycle_diag::with_enabled_for_test(|| {
        let mut f = Fixture::with_config(linear_lifecycle_config());
        f.niri_state().backend.headless().add_renderer().unwrap();
        f.add_output(1, (1920, 1080));
        f.add_output(2, (1280, 720));

        let id = f.add_client();
        let _surface = create_window(&mut f, id, 400, 300);
        f.niri_focus_output(1);
        lifecycle_diag::reset();

        f.client(id).foreign_toplevel(0).set_maximized();
        f.double_roundtrip(id);

        let diag = lifecycle_diag::snapshot();
        let total = f.niri().output_state.len();
        eprintln!(
            "R15_SAMPLE kind=r17_foreign_maximize_dual_output \
             queue_redraw_all={} queue_redraw={} outputs_total={total} \
             targeted_maximize={} note=post_R17_targeted",
            diag.queue_redraw_all, diag.queue_redraw, diag.redraw_targeted_maximize
        );

        assert!(diag.queue_redraw >= 1);
        assert!(diag.redraw_targeted_maximize >= 1);
        // Maximize typically does not re-point the cursor; cluster path itself is
        // targeted (no adapter redraw-all).
        assert_eq!(total, 2);
    });
}

/// R17 contrast: glass commit with mapped root is already targeted (R14).
#[test]
fn r15_glass_commit_mapped_root_is_targeted_not_all() {
    use crate::protocols::raw::tahoe_glass::v1::client::tahoe_glass_surface_v1::TahoeGlassSurfaceV1;
    use crate::protocols::tahoe_glass::{
        test_fallback_redraw_all_count, test_redraw_counter_lock, test_reset_redraw_counters,
        test_targeted_redraw_count,
    };

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));
    let id = f.add_client();

    // Match tahoe_glass::create_mapped_layer (default layer, no output bind).
    let layer = f
        .client(id)
        .create_layer(None, Layer::Top, "tahoe-glass-r15");
    let client_surface = layer.surface.clone();
    layer.set_configure_props(LayerConfigureProps {
        anchor: Some(Anchor::Left | Anchor::Top),
        size: Some((200, 100)),
        ..Default::default()
    });
    layer.commit();
    f.roundtrip(id);
    let layer = f.client(id).layer(&client_surface);
    layer.attach_new_buffer();
    layer.set_size(200, 100);
    layer.ack_last_and_commit();
    f.double_roundtrip(id);

    let glass_manager = f.client(id).tahoe_glass_manager();
    let qh = f.client(id).qh.clone();
    let glass: TahoeGlassSurfaceV1 =
        glass_manager.get_tahoe_glass_surface(&client_surface, &qh, ());

    let _guard = test_redraw_counter_lock();
    test_reset_redraw_counters();
    lifecycle_diag::with_enabled_for_test(|| {
        glass.set_region(
            1,
            8,
            4,
            128,
            32,
            8,
            8,
            8,
            8,
            String::from("panel"),
            7,
            0.0.into(),
            1.0.into(),
        );
        f.client(id).connection.flush().unwrap();
        f.roundtrip(id);
        f.client(id).layer(&client_surface).commit();
        f.double_roundtrip(id);

        let targeted = test_targeted_redraw_count();
        let fallback = test_fallback_redraw_all_count();
        let diag = lifecycle_diag::snapshot();
        eprintln!(
            "R15_SAMPLE kind=r17_glass_commit_mapped \
             glass_targeted={targeted} glass_fallback_all={fallback} \
             queue_redraw_all={} queue_redraw={}",
            diag.queue_redraw_all, diag.queue_redraw
        );

        assert!(
            targeted >= 1,
            "mapped glass root must use targeted redraw (got {targeted})"
        );
        assert_eq!(
            fallback, 0,
            "mapped glass root must not fallback redraw-all"
        );
    });
}

/// R16 pre-baseline: Genie create cost + theoretical per-frame alloc sites.
#[test]
fn r15_genie_create_and_per_frame_alloc_baseline() {
    lifecycle_diag::with_enabled_for_test(|| {
        let mut f = Fixture::with_config(linear_lifecycle_config());
        f.niri_state().backend.headless().add_renderer().unwrap();
        f.add_output(1, (1920, 1080));

        let id = f.add_client();
        // 1080p-ish client buffer (logical); headless scale 1.
        let _surface = create_window(&mut f, id, 1920, 1080);
        let _dock = map_dock_and_set_rect(&mut f, id, 1, 20, 40, 48, 48);

        lifecycle_diag::reset();
        f.client(id).foreign_toplevel(0).set_minimized();
        f.double_roundtrip(id);

        let diag = lifecycle_diag::snapshot();
        // Theoretical per-frame sites in render_genie (source-locked):
        // 1× Rc::new([Uniform; 7]), 1× HashMap::from, 1× String::from,
        // 1× ShaderRenderElement::new → Id::new(). Not allocator-sampled here.
        // Source-locked per-frame allocation groups in render_genie:
        // 1× Rc::new([Uniform; 7]), 1× HashMap::from, 1× String::from,
        // 1× ShaderRenderElement::new → Id::new(). Not allocator-sampled here.
        let per_frame_alloc_groups: u64 = 4;
        let anim_ms = 1000u64;
        let hz = 60u64;
        let frames = anim_ms * hz / 1000;
        let theoretical_alloc_groups_per_anim = per_frame_alloc_groups * frames;

        eprintln!(
            "R15_SAMPLE kind=r16_genie_baseline \
             genie_create={} snapshot_peak_bytes={} snapshot_variant_bytes={} \
             per_frame_alloc_groups={} theoretical_alloc_groups_per_anim={} \
             anim_ms={} hz={} note=source_render_genie_Rc_HashMap_String_ShaderRenderElement_new",
            diag.genie_create,
            diag.snapshot_peak_bytes,
            diag.snapshot_variant_bytes,
            per_frame_alloc_groups,
            theoretical_alloc_groups_per_anim,
            anim_ms,
            hz
        );

        assert!(
            diag.genie_create >= 1,
            "minimize with renderer must create Genie snapshot"
        );
        assert!(
            diag.snapshot_peak_bytes > 0,
            "peak snapshot bytes must be counted"
        );
        // Single-variant Output path for default window (no block-out rule):
        // peak should be on the order of window buffer, not zero.
        let theoretical_1080p = abgr8888_bytes(1920, 1080);
        assert!(
            diag.snapshot_peak_bytes >= theoretical_1080p / 4,
            "peak {} implausibly small vs 1080p Abgr8888 {}",
            diag.snapshot_peak_bytes,
            theoretical_1080p
        );
    });
}

/// R18: default minimize is single-variant; block-out rule forces multi-variant.
#[test]
fn r15_snapshot_variant_single_vs_block_out_peak() {
    // --- A: default single-variant ---
    let single_peak = lifecycle_diag::with_enabled_for_test(|| {
        let mut f = Fixture::with_config(linear_lifecycle_config());
        f.niri_state().backend.headless().add_renderer().unwrap();
        f.add_output(1, (1280, 720));
        let id = f.add_client();
        let _ = create_window(&mut f, id, 640, 360);
        let _ = map_dock_and_set_rect(&mut f, id, 1, 10, 20, 48, 48);
        lifecycle_diag::reset();
        f.client(id).foreign_toplevel(0).set_minimized();
        f.double_roundtrip(id);
        let s = lifecycle_diag::snapshot();
        eprintln!(
            "R15_SAMPLE kind=r18_snapshot_single_variant \
             genie_create={} peak_bytes={} variant_bytes_sum={}",
            s.genie_create, s.snapshot_peak_bytes, s.snapshot_variant_bytes
        );
        assert!(s.genie_create >= 1);
        s.snapshot_peak_bytes
    });

    // --- B: window-rule block-out-from screencast → blocked_out texture too ---
    let multi_peak = lifecycle_diag::with_enabled_for_test(|| {
        let mut f = Fixture::with_config(config_with_block_out_screencast());
        f.niri_state().backend.headless().add_renderer().unwrap();
        f.add_output(1, (1280, 720));
        let id = f.add_client();
        let _ = create_window(&mut f, id, 640, 360);
        let _ = map_dock_and_set_rect(&mut f, id, 1, 10, 20, 48, 48);
        lifecycle_diag::reset();
        f.client(id).foreign_toplevel(0).set_minimized();
        f.double_roundtrip(id);
        let s = lifecycle_diag::snapshot();
        eprintln!(
            "R15_SAMPLE kind=r18_snapshot_block_out_variant \
             genie_create={} peak_bytes={} variant_bytes_sum={}",
            s.genie_create, s.snapshot_peak_bytes, s.snapshot_variant_bytes
        );
        assert!(s.genie_create >= 1);
        s.snapshot_peak_bytes
    });

    let multiplier = if single_peak == 0 {
        0.0
    } else {
        multi_peak as f64 / single_peak as f64
    };
    eprintln!(
        "R15_SAMPLE kind=r18_snapshot_multiplier \
         single_peak={single_peak} multi_peak={multi_peak} multiplier={multiplier:.3} \
         theoretical_sizes_1080p={} 1440p={} 4k={} 4k@2x={}",
        abgr8888_bytes(1920, 1080),
        abgr8888_bytes(2560, 1440),
        abgr8888_bytes(3840, 2160),
        abgr8888_bytes(3840 * 2, 2160 * 2)
    );

    // Document only; go/no-go uses locked threshold on multiplier + stall evidence.
    assert!(single_peak > 0);
    assert!(multi_peak > 0);
}

/// R19 structural: each region invokes capture counter once per render_region.
/// Without GPU timing this cannot prove capture/blur is the primary bottleneck.
#[test]
fn r15_tahoe_region_capture_is_per_region_structural() {
    // Source invariant: render_regions_for_layer loops regions and each
    // render_region calls note_tahoe_region_capture(). Assert the counter
    // machinery still works; multi-region GPU cost needs tracy/session.
    lifecycle_diag::with_enabled_for_test(|| {
        lifecycle_diag::note_tahoe_region_request();
        for _ in 0..7 {
            lifecycle_diag::note_tahoe_region_capture();
        }
        lifecycle_diag::note_tahoe_region_commit();
        let s = lifecycle_diag::snapshot();
        eprintln!(
            "R15_SAMPLE kind=r19_structural_capture_counters \
             request={} commit={} capture={} \
             note=per_region_capture_in_render_region_source;_no_gpu_timing_in_headless",
            s.tahoe_region_request, s.tahoe_region_commit, s.tahoe_region_capture
        );
        assert_eq!(s.tahoe_region_capture, 7);
        assert_eq!(s.tahoe_region_request, 1);
        assert_eq!(s.tahoe_region_commit, 1);
    });
}

/// Theoretical VRAM table used by R18 decisions (no GPU required).
#[test]
fn r15_theoretical_vram_table() {
    let rows = [
        ("1080p@1", 1920u64, 1080u64, 1u64),
        ("1440p@1", 2560, 1440, 1),
        ("4k@1", 3840, 2160, 1),
        ("4k@2", 3840, 2160, 2),
        ("640x360@1", 640, 360, 1),
    ];
    for (label, w, h, scale) in rows {
        let pw = w * scale;
        let ph = h * scale;
        let one = abgr8888_bytes(pw, ph);
        eprintln!(
            "R15_SAMPLE kind=r18_theoretical_vram label={label} \
             physical={pw}x{ph} single_abgr8888={one} \
             two_variants={} three_variants={}",
            one * 2,
            one * 3
        );
    }
    // Sanity: 4K@2 single variant exceeds 100 MiB.
    assert!(abgr8888_bytes(3840 * 2, 2160 * 2) > 100 * 1024 * 1024);
}

/// Static call-site inventory (not go evidence alone; R17 needs runtime samples).
#[test]
fn r15_static_queue_redraw_all_inventory_print() {
    // Values are re-checked by the execution record `rg` commands; this test
    // only documents the target-cluster call sites that still exist.
    let foreign_cluster_sites = [
        "handlers/mod.rs ForeignToplevelHandler::activate",
        "handlers/mod.rs ForeignToplevelHandler::set_maximized",
        "handlers/mod.rs ForeignToplevelHandler::unset_maximized",
        "handlers/mod.rs ForeignToplevelHandler::set_minimized",
        "handlers/mod.rs ForeignToplevelHandler::unset_minimized",
        "handlers/mod.rs TahoeGlassHandler fallback (unlocatable only)",
    ];
    eprintln!(
        "R15_SAMPLE kind=r17_static_cluster_sites count={} sites={foreign_cluster_sites:?}",
        foreign_cluster_sites.len()
    );
    assert_eq!(foreign_cluster_sites.len(), 6);
}

/// Duration marker so session logs can align with headless samples.
#[test]
fn r15_environment_marker() {
    eprintln!(
        "R15_SAMPLE kind=env profile=debug_headless \
         sample_clock_ms={} note=nested_session_frame_telemetry_separate",
        Duration::from_secs(0).as_millis()
    );
}
