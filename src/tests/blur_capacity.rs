//! T10 blur texture capacity reuse integration tests.
//!
//! These tests drive the real GL blur authority (`Blur::prepare_textures` /
//! `Blur::render`) on a surfaceless EGL renderer and assert the capacity
//! policy the old implementation violates:
//!
//! - continuous 1px/2px/8px resize steps must not recreate the texture pyramid on every step (old
//!   implementation reallocated on any size change),
//! - textures larger than the source keep sampling, clamping and viewports on the *active*
//!   sub-rectangle so stale capacity pixels never leak into the result,
//! - 4K -> small -> 4K oscillation, pass/format/context/shared-reference switches and renderer
//!   reset stay bounded and return the global byte budget to baseline,
//! - both the per-pyramid and the global byte caps are hard limits.

use std::iter::once;

use smithay::backend::allocator::Fourcc;
use smithay::backend::egl::native::EGLSurfacelessDisplay;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{Bind as _, Color32F, ExportMem as _, Offscreen as _};
use smithay::utils::{Point, Scale, Size, Transform};

/// Serialized with the blur.rs unit tests through the shared budget lock so
/// the process-global byte-budget accounting never interleaves.
use crate::render_helpers::blur::BLUR_BUDGET_TEST_LOCK as BLUR_BUDGET_LOCK;
use crate::render_helpers::blur::{retained_blur_texture_bytes, Blur, BlurOptions, BlurTrace};
use crate::render_helpers::render_to_texture;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::utils::lifecycle_diag;

fn make_renderer() -> GlesRenderer {
    let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }
        .expect("creating a surfaceless EGL display");
    let context = EGLContext::new(&display).expect("creating an EGL context");
    let mut renderer = unsafe { GlesRenderer::new(context) }.expect("creating a GLES renderer");
    crate::render_helpers::shaders::init(&mut renderer);
    renderer
}

fn options(passes: u8) -> BlurOptions {
    BlurOptions {
        passes,
        offset: 4.,
        downsample_shift: 0,
    }
}

/// Run the whole test inside the serialized lifecycle-diag window so blur
/// counters are exact (only the current test can be counting).
fn with_diag<R>(f: impl FnOnce() -> R) -> R {
    lifecycle_diag::with_enabled_for_test(f)
}

/// Prepare a blur pyramid for `size`, counting every texture the create
/// closure is asked for.
fn prepare_counting(
    blur: &mut Blur,
    renderer: &mut GlesRenderer,
    size: Size<i32, smithay::utils::Buffer>,
    passes: u8,
    allocations: &mut Vec<Size<i32, smithay::utils::Buffer>>,
) {
    let source = renderer
        .create_buffer(Fourcc::Abgr8888, size)
        .expect("creating the blur source texture");
    blur.prepare_textures(
        |fourcc, size| {
            allocations.push(size);
            renderer.create_buffer(fourcc, size)
        },
        &source,
        options(passes),
        BlurTrace::default(),
    )
    .expect("preparing blur textures");
}

/// A solid-color texture rendered through the real GL pipeline.
fn solid_texture(
    renderer: &mut GlesRenderer,
    w: i32,
    h: i32,
    color: Color32F,
) -> smithay::backend::renderer::gles::GlesTexture {
    let buffer = SolidColorBuffer::new((f64::from(w), f64::from(h)), color);
    let element =
        SolidColorRenderElement::from_buffer(&buffer, Point::from((0., 0.)), 1., Kind::Unspecified);
    let size = Size::from((w, h));
    render_to_texture(
        renderer,
        size,
        Scale::from(1.),
        Transform::Normal,
        Fourcc::Abgr8888,
        once(element),
    )
    .expect("rendering the solid source")
    .0
}

/// Read the current content of a texture back to RGBA bytes.
fn read_texture(
    renderer: &mut GlesRenderer,
    texture: &smithay::backend::renderer::gles::GlesTexture,
) -> Vec<u8> {
    let mut texture = texture.clone();
    let target = renderer.bind(&mut texture).expect("binding texture");
    let mapping = crate::render_helpers::copy_framebuffer(renderer, &target, Fourcc::Abgr8888)
        .expect("copying framebuffer");
    renderer
        .map_texture(&mapping)
        .expect("mapping texture")
        .to_vec()
}

/// A10.1: a fixed scene whose surface size is stepped 1px/2px/8px at a time
/// must only cross capacity bucket boundaries, not recreate the pyramid on
/// every step. The old implementation recreated every level on any size
/// change (512 steps -> ~2048 texture allocations).
#[test]
fn continuous_resize_stays_within_capacity_buckets() {
    let _lock = BLUR_BUDGET_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut renderer = make_renderer();
    let mut blur = Blur::new(&mut renderer).expect("blur program");

    for step in [1, 2, 8] {
        let mut allocations = Vec::new();
        let mut width = 1;
        while width <= 512 {
            prepare_counting(
                &mut blur,
                &mut renderer,
                Size::from((width, 1080)),
                3,
                &mut allocations,
            );
            width += step;
        }
        let prepares = (0..512).step_by(step as usize).count();
        assert!(
            allocations.len() <= 64,
            "1px/2px/8px resize must cross only bucket boundaries: \
             {prepares} prepares caused {} allocations",
            allocations.len()
        );
        assert!(
            allocations.len() > 0,
            "the first prepare must allocate the initial pyramid"
        );
    }
}

/// A10.3: 4K -> small -> 4K oscillation. The small phase reuses the 4K
/// capacity for 7 prepares (shrink hysteresis), shrinks on the 8th, and the
/// final 4K phase recreates once. Total: 4 + 4 + 4 = 12 allocations.
#[test]
fn four_k_oscillation_shrinks_only_after_hysteresis_and_grows_back_bounded() {
    let _lock = BLUR_BUDGET_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut renderer = make_renderer();
    let mut blur = Blur::new(&mut renderer).expect("blur program");
    let mut allocations = Vec::new();

    prepare_counting(
        &mut blur,
        &mut renderer,
        Size::from((4096, 2160)),
        3,
        &mut allocations,
    );
    assert_eq!(allocations.len(), 4, "4K pyramid must allocate once");

    for _ in 0..7 {
        prepare_counting(
            &mut blur,
            &mut renderer,
            Size::from((200, 100)),
            3,
            &mut allocations,
        );
    }
    assert_eq!(
        allocations.len(),
        4,
        "smaller surfaces must reuse the 4K capacity during the hysteresis window"
    );

    prepare_counting(
        &mut blur,
        &mut renderer,
        Size::from((200, 100)),
        3,
        &mut allocations,
    );
    assert_eq!(
        allocations.len(),
        8,
        "the 8th consecutive smaller prepare must shrink the pyramid"
    );

    prepare_counting(
        &mut blur,
        &mut renderer,
        Size::from((4096, 2160)),
        3,
        &mut allocations,
    );
    assert_eq!(
        allocations.len(),
        12,
        "returning to 4K must recreate the pyramid exactly once"
    );
}

/// A10.3: a pass count change is a hard incompatibility; the whole pyramid is
/// recreated, then the new plan is reused.
#[test]
fn pass_count_change_recreates_then_reuses() {
    let _lock = BLUR_BUDGET_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut renderer = make_renderer();
    let mut blur = Blur::new(&mut renderer).expect("blur program");
    let mut allocations = Vec::new();

    prepare_counting(
        &mut blur,
        &mut renderer,
        Size::from((200, 100)),
        3,
        &mut allocations,
    );
    assert_eq!(allocations.len(), 4);

    prepare_counting(
        &mut blur,
        &mut renderer,
        Size::from((200, 100)),
        4,
        &mut allocations,
    );
    assert_eq!(
        allocations.len(),
        9,
        "pass count change must recreate all 5 levels of the new pyramid"
    );

    prepare_counting(
        &mut blur,
        &mut renderer,
        Size::from((200, 100)),
        4,
        &mut allocations,
    );
    assert_eq!(allocations.len(), 9, "the new pass count must be reused");
}

/// A10.3: a texture format change is a hard incompatibility; the offending
/// level (and everything below it) is recreated, then reuse resumes.
#[test]
fn format_change_recreates_once_then_reuses() {
    let _lock = BLUR_BUDGET_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut renderer = make_renderer();
    let mut blur = Blur::new(&mut renderer).expect("blur program");
    let mut allocations = Vec::new();

    // First allocation round stores a foreign format texture.
    let wrong_format = true;
    let source = renderer
        .create_buffer(Fourcc::Abgr8888, Size::from((200, 100)))
        .expect("creating the blur source texture");
    blur.prepare_textures(
        |fourcc, size| {
            let fourcc = if wrong_format {
                Fourcc::Xbgr8888
            } else {
                fourcc
            };
            allocations.push(size);
            renderer.create_buffer(fourcc, size)
        },
        &source,
        options(3),
        BlurTrace::default(),
    )
    .expect("preparing blur textures");
    assert_eq!(allocations.len(), 4);

    // The stored format is incompatible: recreate, now with the right format.
    blur.prepare_textures(
        |fourcc, size| {
            allocations.push(size);
            renderer.create_buffer(fourcc, size)
        },
        &source,
        options(3),
        BlurTrace::default(),
    )
    .expect("preparing blur textures");
    assert_eq!(
        allocations.len(),
        8,
        "a format change must recreate the incompatible levels"
    );

    // Reuse resumes once the format matches.
    blur.prepare_textures(
        |fourcc, size| {
            allocations.push(size);
            renderer.create_buffer(fourcc, size)
        },
        &source,
        options(3),
        BlurTrace::default(),
    )
    .expect("preparing blur textures");
    assert_eq!(allocations.len(), 8, "matching format must be reused");
}

/// A10.3: a shared (non-unique) reference to the output texture is a hard
/// incompatibility; level 0 (and everything below it) is recreated. This is
/// the renderer-reset-adjacent path: any consumer holding the output across
/// prepares must not leave stale capacity behind.
#[test]
fn shared_output_reference_recreates_the_pyramid() {
    let _lock = BLUR_BUDGET_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut renderer = make_renderer();
    let mut blur = Blur::new(&mut renderer).expect("blur program");
    let mut allocations = Vec::new();

    prepare_counting(
        &mut blur,
        &mut renderer,
        Size::from((200, 100)),
        3,
        &mut allocations,
    );
    assert_eq!(allocations.len(), 4);

    let source = renderer
        .create_buffer(Fourcc::Abgr8888, Size::from((200, 100)))
        .expect("creating the blur source texture");
    let output = blur
        .render(&mut renderer, &source, options(3))
        .expect("rendering blur");
    assert_eq!(output.active_size, Size::from((200, 100)));

    // The render result holds a clone of level 0: the next prepare must not
    // reuse a texture with a non-unique reference.
    prepare_counting(
        &mut blur,
        &mut renderer,
        Size::from((200, 100)),
        3,
        &mut allocations,
    );
    assert_eq!(
        allocations.len(),
        8,
        "a shared output reference must force a recreation"
    );

    drop(output);
    prepare_counting(
        &mut blur,
        &mut renderer,
        Size::from((200, 100)),
        3,
        &mut allocations,
    );
    assert_eq!(
        allocations.len(),
        8,
        "once the reference is gone the pyramid is reused again"
    );
}

/// A10.3: dropping the Blur (renderer reset path: `Inner::new` on a context
/// change) returns the global byte budget to baseline, and a fresh renderer
/// context starts a fresh allocation budget instead of reusing foreign state.
#[test]
fn renderer_reset_releases_global_budget_and_starts_fresh() {
    let _lock = BLUR_BUDGET_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let baseline = retained_blur_texture_bytes();

    let pyramid_bytes = {
        let mut renderer = make_renderer();
        let mut blur = Blur::new(&mut renderer).expect("blur program");
        let mut allocations = Vec::new();
        prepare_counting(
            &mut blur,
            &mut renderer,
            Size::from((4096, 2160)),
            3,
            &mut allocations,
        );
        assert_eq!(allocations.len(), 4);
        let bytes = retained_blur_texture_bytes() - baseline;
        assert!(bytes > 0, "a 4K pyramid must charge the global budget");
        drop(blur);
        assert_eq!(
            retained_blur_texture_bytes(),
            baseline,
            "dropping the blur must release the whole pyramid charge"
        );
        bytes
    };

    let mut renderer = make_renderer();
    let mut blur = Blur::new(&mut renderer).expect("blur program");
    let mut allocations = Vec::new();
    prepare_counting(
        &mut blur,
        &mut renderer,
        Size::from((4096, 2160)),
        3,
        &mut allocations,
    );
    assert_eq!(
        allocations.len(),
        4,
        "a fresh renderer context must not reuse textures from the old context"
    );
    assert_eq!(
        retained_blur_texture_bytes() - baseline,
        pyramid_bytes,
        "the new context must charge exactly its own pyramid"
    );
}

/// A10.4: a plan whose retained bytes exceed the per-pyramid hard budget is
/// rejected before any GL allocation, without corrupting the blur state.
#[test]
fn per_pyramid_budget_rejects_oversized_plan_without_state_corruption() {
    let _lock = BLUR_BUDGET_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let baseline = retained_blur_texture_bytes();
    let mut renderer = make_renderer();
    let mut blur = Blur::new(&mut renderer).expect("blur program");

    let source = renderer
        .create_buffer(Fourcc::Abgr8888, Size::from((8192, 8192)))
        .expect("creating the blur source texture");
    let mut allocations = Vec::new();
    let result = blur.prepare_textures(
        |fourcc, size| {
            allocations.push(size);
            renderer.create_buffer(fourcc, size)
        },
        &source,
        options(3),
        BlurTrace::default(),
    );
    assert!(result.is_err(), "an over-budget pyramid must be rejected");
    assert!(allocations.is_empty(), "no GL allocation may be attempted");
    assert_eq!(retained_blur_texture_bytes(), baseline);

    // The blur remains usable for a legitimate plan.
    prepare_counting(
        &mut blur,
        &mut renderer,
        Size::from((200, 100)),
        3,
        &mut allocations,
    );
    assert_eq!(allocations.len(), 4);
}

/// A10.4: the global byte cap rejects a new pyramid while existing pyramids
/// hold the budget, and the same request succeeds once one releases it.
///
/// Three 3-pass pyramids of a 8192x4480 surface charge ~186 MiB each; two fit
/// under the 512 MiB global cap, the third must be rejected.
#[test]
fn global_budget_blocks_until_existing_pyramids_release() {
    let _lock = BLUR_BUDGET_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let failures = with_diag(|| {
        let mut renderer = make_renderer();
        let mut blur1 = Blur::new(&mut renderer).expect("blur program");
        let mut blur2 = Blur::new(&mut renderer).expect("blur program");
        let mut blur3 = Blur::new(&mut renderer).expect("blur program");
        let mut allocations = Vec::new();

        prepare_counting(
            &mut blur1,
            &mut renderer,
            Size::from((8192, 4480)),
            3,
            &mut allocations,
        );
        prepare_counting(
            &mut blur2,
            &mut renderer,
            Size::from((8192, 4480)),
            3,
            &mut allocations,
        );
        assert_eq!(allocations.len(), 8);

        // A third pyramid would charge ~186 MiB more than the 512 MiB cap.
        let source = renderer
            .create_buffer(Fourcc::Abgr8888, Size::from((8192, 4480)))
            .expect("creating the blur source texture");
        let result = blur3.prepare_textures(
            |fourcc, size| {
                allocations.push(size);
                renderer.create_buffer(fourcc, size)
            },
            &source,
            options(3),
            BlurTrace::default(),
        );
        assert!(
            result.is_err(),
            "the global cap must reject a third 8K pyramid"
        );
        assert_eq!(
            allocations.len(),
            8,
            "the rejected pyramid must not have allocated anything"
        );

        drop(blur1);

        // Releasing one pyramid frees enough budget for the third to succeed.
        blur3
            .prepare_textures(
                |fourcc, size| {
                    allocations.push(size);
                    renderer.create_buffer(fourcc, size)
                },
                &source,
                options(3),
                BlurTrace::default(),
            )
            .expect("retry after release must succeed");
        assert_eq!(allocations.len(), 12);

        lifecycle_diag::snapshot().blur_budget_reservation_failures
    });
    assert_eq!(failures, 1, "exactly one budget reservation must fail");
}

/// A10.2: when the capacity texture is larger than the source, blur passes
/// write only the active sub-rectangle and the output exposes only it — a
/// reused pyramid must never leak pixels from a previous content into the
/// active area.
#[test]
fn reused_capacity_rewrites_active_pixels_without_leaking_old_content() {
    let _lock = BLUR_BUDGET_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut renderer = make_renderer();
    let mut blur = Blur::new(&mut renderer).expect("blur program");
    let mut allocations = Vec::new();

    // 100x100 and 120x120 share the same 128x128 capacity bucket.
    prepare_counting(
        &mut blur,
        &mut renderer,
        Size::from((100, 100)),
        3,
        &mut allocations,
    );
    assert_eq!(allocations.len(), 4);

    let red_source = solid_texture(&mut renderer, 100, 100, Color32F::new(1., 0., 0., 1.));
    let red = blur
        .render(&mut renderer, &red_source, options(3))
        .expect("rendering the red blur");
    assert_eq!(red.active_size, Size::from((100, 100)));
    assert_eq!(
        red.source_rect().size,
        Size::<f64, smithay::utils::Buffer>::from((100., 100.)),
        "the output must expose only the active sub-rectangle"
    );

    // A10.2: source rects used by consumers (damage/viewport mapping) are
    // clamped into the active area; stale capacity pixels are never sampled.
    let inside = smithay::utils::Rectangle::new(
        smithay::utils::Point::from((10., 10.)),
        Size::<f64, smithay::utils::Buffer>::from((50., 40.)),
    );
    assert_eq!(red.map_source_rect(inside), inside);
    let straddling = smithay::utils::Rectangle::new(
        smithay::utils::Point::from((90., 60.)),
        Size::<f64, smithay::utils::Buffer>::from((40., 40.)),
    );
    assert_eq!(
        red.map_source_rect(straddling),
        smithay::utils::Rectangle::new(
            smithay::utils::Point::from((90., 60.)),
            Size::<f64, smithay::utils::Buffer>::from((10., 40.)),
        ),
        "a rect straddling the active boundary must be clamped into it"
    );
    let outside = smithay::utils::Rectangle::new(
        smithay::utils::Point::from((200., 0.)),
        Size::<f64, smithay::utils::Buffer>::from((10., 10.)),
    );
    assert_eq!(
        red.map_source_rect(outside),
        smithay::utils::Rectangle::from_size(Size::<f64, smithay::utils::Buffer>::default()),
        "a rect fully outside the active area must never be sampled"
    );

    let pixels = read_texture(&mut renderer, &red.texture);
    for y in 0..100 {
        for x in 0..100 {
            let px = &pixels[(y * 128 + x) * 4..][..4];
            assert!(
                px[0] > px[2],
                "red content must fill the active area (px {x},{y}: {px:?})"
            );
        }
    }
    // The render result holds a clone of the output texture; releasing it
    // restores the unique reference before the next prepare.
    drop(red);

    // Same bucket: no reallocation, but the next render must overwrite the
    // whole active area — no red remnants anywhere inside 120x120.
    prepare_counting(
        &mut blur,
        &mut renderer,
        Size::from((120, 120)),
        3,
        &mut allocations,
    );
    assert_eq!(
        allocations.len(),
        4,
        "120x120 must reuse the 128x128 capacity bucket"
    );
    let blue_source = solid_texture(&mut renderer, 120, 120, Color32F::new(0., 0., 1., 1.));
    let blue = blur
        .render(&mut renderer, &blue_source, options(3))
        .expect("rendering the blue blur");
    assert_eq!(blue.active_size, Size::from((120, 120)));

    let pixels = read_texture(&mut renderer, &blue.texture);
    for y in 0..120 {
        for x in 0..120 {
            let px = &pixels[(y * 128 + x) * 4..][..4];
            assert!(
                px[2] > px[0],
                "reused capacity must not leak the previous red content (px {x},{y}: {px:?})"
            );
        }
    }
}
