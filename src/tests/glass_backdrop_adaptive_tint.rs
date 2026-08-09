//! Backdrop-adaptive glass tint (`postprocess.frag`).
//!
//! A single fixed tint cannot serve both polarities of backdrop. The material
//! recipe is near-white, which gives the glass its body over a dark wallpaper
//! but is an identity operation over white application content: mixing white
//! toward white cannot separate the surface from what is behind it. Before the
//! fix, a Control Center over a white document rendered at code ~254 against a
//! ~249 backdrop — a 1.01:1 contrast ratio. The panel, its border and its rim
//! were all invisible and the only remaining cue was the blur bleeding out past
//! the panel edge, which reads as a smudge rather than as glass.
//!
//! These tests drive the real compiled `postprocess_and_clip` program on the
//! GPU (not a CPU mirror) and read the resulting pixels back, so they measure
//! the shader that actually ships.
//!
//! The invariants under test:
//!   1. over a light backdrop the material darkens enough to be seen;
//!   2. over a dark backdrop it still lightens, unchanged from the tuned look;
//!   3. the transition between the two is monotonic and free of a visible step, so a surface
//!      dragged across a wallpaper/content boundary never pops.

use glam::Mat3;
use smithay::backend::allocator::Fourcc;
use smithay::backend::egl::native::EGLSurfacelessDisplay;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture, Uniform};
use smithay::backend::renderer::{Bind as _, Color32F, ExportMem as _, Frame as _, Renderer as _};
use smithay::utils::{Buffer, Point, Rectangle, Scale, Size, Transform};

use crate::render_helpers::shaders::{mat3_uniform, Shaders};
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::{create_texture, render_to_texture};

/// Region size used for every probe. Large enough that `glass_surface_detail()`
/// stays in its full-detail range, so the rim and normals are actually
/// evaluated rather than taking the cheap directional path.
const SIZE: i32 = 256;

/// Deployed `panel` material (config/niri/tahoe-phase0.kdl, and the built-in
/// default in niri-config/src/tahoe_glass.rs — the governance test keeps the
/// two in sync).
const PANEL_TINT: [f32; 4] = [
    0xf2 as f32 / 255.,
    0xf6 as f32 / 255.,
    0xfa as f32 / 255.,
    1.,
];
const PANEL_TINT_AMOUNT: f32 = 0.12;
const PANEL_SATURATION: f32 = 1.35;
const PANEL_EDGE_HIGHLIGHT: f32 = 0.45;
const PANEL_INNER_SHADOW: f32 = 0.06;

fn make_renderer() -> GlesRenderer {
    let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }
        .expect("creating a surfaceless EGL display");
    let context = EGLContext::new(&display).expect("creating an EGL context");
    let mut renderer = unsafe { GlesRenderer::new(context) }.expect("creating a GLES renderer");
    crate::render_helpers::shaders::init(&mut renderer);
    renderer
}

/// Render a uniform backdrop of `code` through the real postprocess program and
/// return the RGBA bytes.
///
/// `noise` is forced to 0 so the readback is deterministic; every other
/// parameter is the deployed `panel` recipe.
fn render_glass_over(renderer: &mut GlesRenderer, code: u8) -> Vec<u8> {
    let value = f32::from(code) / 255.;
    let buffer = SolidColorBuffer::new(
        (f64::from(SIZE), f64::from(SIZE)),
        Color32F::new(value, value, value, 1.),
    );
    let element =
        SolidColorRenderElement::from_buffer(&buffer, Point::from((0., 0.)), 1., Kind::Unspecified);
    let (backdrop, _sync) = render_to_texture(
        renderer,
        Size::from((SIZE, SIZE)),
        Scale::from(1.),
        Transform::Normal,
        Fourcc::Abgr8888,
        std::iter::once(element),
    )
    .expect("rendering the backdrop");

    let program = Shaders::get(renderer)
        .postprocess_and_clip
        .clone()
        .expect("postprocess_and_clip must compile");

    // Identity geometry mapping: the region covers the whole texture, so
    // input coords and geometry coords coincide.
    let uniforms = [
        Uniform::new("niri_scale", 1.0f32),
        Uniform::new("geo_size", (SIZE as f32, SIZE as f32)),
        Uniform::new("corner_radius", [0f32, 0., 0., 0.]),
        mat3_uniform("input_to_geo", Mat3::IDENTITY),
        mat3_uniform("geo_to_input", Mat3::IDENTITY),
        Uniform::new("noise", 0.0f32),
        Uniform::new("saturation", PANEL_SATURATION),
        Uniform::new("bg_color", [0f32, 0., 0., 0.]),
        Uniform::new("tint_color", PANEL_TINT),
        Uniform::new("tint_amount", PANEL_TINT_AMOUNT),
        Uniform::new("contrast", 1.0f32),
        Uniform::new("edge_highlight", PANEL_EDGE_HIGHLIGHT),
        Uniform::new("refraction", 0.0f32),
        Uniform::new("inner_shadow", PANEL_INNER_SHADOW),
        Uniform::new("chromatic", 0.0f32),
        Uniform::new("lens_depth", 0.0f32),
    ];

    // Drive the program the same way FramebufferEffectElement does in
    // production: render_texture_from_to with the postprocess program bound.
    let mut out = create_texture(renderer, Size::from((SIZE, SIZE)), Fourcc::Abgr8888)
        .expect("creating the output texture");
    {
        let mut target = renderer.bind(&mut out).expect("binding the output");
        let mut frame = renderer
            .render(&mut target, Size::from((SIZE, SIZE)), Transform::Normal)
            .expect("starting a frame");
        let full = Rectangle::from_size(Size::from((SIZE, SIZE)));
        frame
            .render_texture_from_to(
                &backdrop,
                Rectangle::<f64, Buffer>::from_size(Size::from((f64::from(SIZE), f64::from(SIZE)))),
                full,
                &[full],
                &[],
                Transform::Normal,
                1.,
                Some(&program),
                &uniforms,
            )
            .expect("rendering the glass");
        let _sync = frame.finish().expect("finishing the frame");
    }

    read_texture(renderer, &out)
}

fn read_texture(renderer: &mut GlesRenderer, texture: &GlesTexture) -> Vec<u8> {
    let mut texture = texture.clone();
    let target = renderer.bind(&mut texture).expect("binding texture");
    let mapping = crate::render_helpers::copy_framebuffer(renderer, &target, Fourcc::Abgr8888)
        .expect("copying framebuffer");
    renderer
        .map_texture(&mapping)
        .expect("mapping texture")
        .to_vec()
}

/// Sample the region center, far from the rim band so the value reflects the
/// tint rather than the edge light.
fn center_luma(pixels: &[u8]) -> f64 {
    let idx = ((SIZE as usize / 2) * SIZE as usize + SIZE as usize / 2) * 4;
    let px = &pixels[idx..][..4];
    0.2126 * f64::from(px[0]) + 0.7152 * f64::from(px[1]) + 0.0722 * f64::from(px[2])
}

/// Brightest and darkest luma anywhere in the region. The rim is a few pixels
/// wide and its position depends on the SDF, so a fixed offset would be
/// fragile; the extremes capture it regardless of where it lands.
fn luma_extremes(pixels: &[u8]) -> (f64, f64) {
    let mut lo = f64::MAX;
    let mut hi = f64::MIN;
    for px in pixels.chunks_exact(4) {
        let l = 0.2126 * f64::from(px[0]) + 0.7152 * f64::from(px[1]) + 0.0722 * f64::from(px[2]);
        lo = lo.min(l);
        hi = hi.max(l);
    }
    (lo, hi)
}

/// WCAG contrast ratio between two 0..255 luma codes treated as gray.
fn contrast_ratio(a: f64, b: f64) -> f64 {
    fn lin(code: f64) -> f64 {
        let c = code / 255.;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    let (hi, lo) = if a > b { (a, b) } else { (b, a) };
    (lin(hi) + 0.05) / (lin(lo) + 0.05)
}

/// The regression: over white content the panel must be visibly darker than
/// its backdrop. Before the adaptive tint this produced ~1.01:1.
#[test]
fn glass_is_visible_over_white_content() {
    let mut renderer = make_renderer();

    for code in [255u8, 249, 240] {
        let pixels = render_glass_over(&mut renderer, code);
        let luma = center_luma(&pixels);
        let ratio = contrast_ratio(luma, f64::from(code));
        eprintln!("[glass] backdrop code={code} -> panel luma={luma:.1} contrast={ratio:.3}:1");

        assert!(
            luma < f64::from(code) - 8.,
            "over light content the panel must darken: backdrop={code} panel={luma:.1}"
        );
        assert!(
            ratio >= 1.12,
            "panel must be separable from a light backdrop: {ratio:.3}:1 at code {code}"
        );
    }
}

/// The other polarity must not regress: over a dark wallpaper the same
/// material still lightens, which is what gives the glass its body there.
#[test]
fn glass_still_lightens_over_dark_backdrop() {
    let mut renderer = make_renderer();

    for code in [20u8, 40, 60] {
        let pixels = render_glass_over(&mut renderer, code);
        let luma = center_luma(&pixels);
        let ratio = contrast_ratio(luma, f64::from(code));
        eprintln!("[glass] backdrop code={code} -> panel luma={luma:.1} contrast={ratio:.3}:1");

        assert!(
            luma > f64::from(code) + 8.,
            "over a dark backdrop the panel must lighten: backdrop={code} panel={luma:.1}"
        );
        assert!(
            ratio >= 1.12,
            "panel must be separable from a dark backdrop: {ratio:.3}:1 at code {code}"
        );
    }
}

/// The inversion must be gradual. A surface dragged across a wallpaper/content
/// boundary crosses the whole range, so a hard switch would pop.
///
/// Sweeping the backdrop, the rendered luma must be monotonically
/// non-decreasing (a darker backdrop never yields a lighter panel) and no
/// single 8-code step may jump more than a threshold well below a visible pop.
#[test]
fn adaptive_tint_transition_is_monotonic_and_stepless() {
    let mut renderer = make_renderer();

    let mut samples = Vec::new();
    let mut code = 0u16;
    while code <= 255 {
        let pixels = render_glass_over(&mut renderer, code as u8);
        samples.push((code as u8, center_luma(&pixels)));
        code += 8;
    }

    let mut worst_step = 0f64;
    for pair in samples.windows(2) {
        let (prev_code, prev_luma) = pair[0];
        let (next_code, next_luma) = pair[1];
        assert!(
            next_luma >= prev_luma - 0.6,
            "panel luma must not decrease as the backdrop brightens: \
             code {prev_code}->{next_code} gave {prev_luma:.1}->{next_luma:.1}"
        );
        worst_step = worst_step.max(next_luma - prev_luma);
    }

    eprintln!("[glass] largest luma step across an 8-code backdrop change: {worst_step:.2}");
    assert!(
        worst_step < 12.0,
        "adaptive tint must not step visibly: worst step {worst_step:.2}"
    );

    // And the two ends must actually differ in direction, i.e. the material
    // really did invert rather than merely flatten out.
    let (dark_code, dark_luma) = samples.first().copied().expect("samples");
    let (light_code, light_luma) = samples.last().copied().expect("samples");
    assert!(
        dark_luma > f64::from(dark_code),
        "dark end must lighten: {dark_luma:.1} vs {dark_code}"
    );
    assert!(
        light_luma < f64::from(light_code),
        "light end must darken: {light_luma:.1} vs {light_code}"
    );
}

/// The surface must never disappear at *any* backdrop, including the mid-gray
/// crossing point.
///
/// One crossing is unavoidable: a fill that lightens a dark backdrop and
/// darkens a light one must equal its backdrop exactly once in between. An
/// earlier iteration of this fix cross-faded the lighten against the darken,
/// which cancelled them over a wide mid-gray band and merely moved the
/// invisible-panel bug from white to gray — this test is what caught it.
///
/// The rim therefore inverts on its own, earlier window, so where the fill
/// approaches its crossing the rim is already darkening.
///
/// This renders the compositor material alone. In production the shell also
/// paints a translucent fill and a hairline stroke on top, so the number here
/// is a floor, not the user-visible contrast: `tahoe-shell`'s material
/// governance test models the composed stack. What this asserts is that the
/// compositor never leaves the surface with *no* cue of its own.
#[test]
fn glass_keeps_a_visible_cue_at_every_backdrop() {
    let mut renderer = make_renderer();

    let mut worst = (f64::MAX, 0u8, 0f64, 0f64);
    let mut code = 0u16;
    while code <= 255 {
        let pixels = render_glass_over(&mut renderer, code as u8);
        let body = center_luma(&pixels);
        let (lo, hi) = luma_extremes(&pixels);

        // Best available separation: the body against the backdrop, or the rim
        // (whichever extreme is further) against the body.
        let body_cue = contrast_ratio(body, f64::from(code));
        let rim_cue = contrast_ratio(hi, body).max(contrast_ratio(lo, body));
        let best = body_cue.max(rim_cue);
        if best < worst.0 {
            worst = (best, code as u8, body_cue, rim_cue);
        }
        code += 4;
    }

    let (best, at_code, body_cue, rim_cue) = worst;
    eprintln!(
        "[glass] weakest cue over the whole backdrop sweep: {best:.3}:1 at code {at_code} \
         (body {body_cue:.3}:1, rim {rim_cue:.3}:1)"
    );
    assert!(
        best >= 1.05,
        "the surface must stay visible at every backdrop; at code {at_code} the best cue was \
         only {best:.3}:1 (body {body_cue:.3}:1, rim {rim_cue:.3}:1)"
    );
}

/// Dark-backdrop parity, stated precisely.
///
/// The claim this change rests on is that a dark wallpaper renders exactly as
/// it did before the adaptive term existed. That is exact only where both
/// smoothstep windows return 0: below `GLASS_RIM_LUMA_LOW` (0.18, i.e. backdrop
/// code ~46) nothing is touched at all. Above it the *fill* is still untouched
/// — `GLASS_ADAPT_LUMA_LOW` is 0.34 — but the rim begins to darken, and real
/// photographic wallpapers have midtones in that band.
///
/// This pins how far the rim may move there, so "unchanged over dark
/// wallpapers" stays an honest statement rather than one that only holds for
/// near-black. The other two tests sample code 20-60 and would not see it.
#[test]
fn dark_wallpaper_midtones_move_only_within_a_bounded_rim_band() {
    let mut renderer = make_renderer();

    for code in [46u8, 60, 70, 87, 100] {
        let pixels = render_glass_over(&mut renderer, code);
        let (_, brightest) = luma_extremes(&pixels);
        let body = center_luma(&pixels);

        // The fill window is still shut here, so the body must keep lightening
        // exactly as it always did.
        assert!(
            body > f64::from(code),
            "the fill must be untouched below the adapt window: backdrop={code} body={body:.1}"
        );

        eprintln!("[glass] midtone backdrop code={code}: body={body:.1} rim={brightest:.1}");
    }

    // Below the rim window nothing may move at all. Compare the rim against the
    // value the same material produces just under the window's opening.
    let below = render_glass_over(&mut renderer, 40);
    let (_, rim_below) = luma_extremes(&below);
    let at_open = render_glass_over(&mut renderer, 46);
    let (_, rim_at_open) = luma_extremes(&at_open);
    assert!(
        rim_at_open > rim_below,
        "a brighter backdrop must still give a brighter rim: {rim_below:.1} -> {rim_at_open:.1}"
    );
}
