//! T13 linear-light / high-precision blur evidence gate (A13.1-A13.5).
//!
//! These tests are measurement instruments for the GLASS-01 `PROPOSAL`:
//! "the 8-bit nonlinear dual-Kawase blur produces banding / dark-halo
//! artifacts; moving the pyramid to linear light with a half-float
//! intermediate quantifiably removes them". The roadmap requires evidence
//! *before* any pipeline change:
//!
//! - A13.1: fixed 10-bit gradient / high-contrast edge / wallpaper-proxy scenes quantize the
//!   current 8-bit error against a linear reference (numeric banding metrics, not subjective
//!   screenshots).
//! - A13.2: decode -> blur -> postprocess -> encode happens exactly once per region per frame (no
//!   second conversion in a shared path).
//! - A13.3: half-float renderability capability detection covers the target GPU and the fallback
//!   path is one internal interface.
//! - A13.4: 4K multi-region memory / frame-time / power deltas stay inside the budget recorded at
//!   task start.
//! - A13.5: if the gate does not pass, the task closes as RESOLVED-NO-CODE.
//!
//! The quality measurements come from a deterministic CPU mirror of the GPU
//! pipeline. The mirror reproduces the shaders' sampling geometry (GL_LINEAR
//! texel-center semantics with the `tex_bounds` clamp), kernel weights and
//! per-level 8-bit (resp. 16f) quantization; its headline numbers were
//! re-derived independently and agree with the real GL pipeline on the
//! gamma-space semantics (the shaders average raw sRGB codes — they contain
//! no gamma functions — and a uniform 8-bit code passes through unchanged on
//! the real GPU). The GPU itself is exercised for capability evidence (A13.3)
//! through a surfaceless EGL renderer creating, rendering to and reading back
//! an RGBA16F texture — the renderable/store path a half-float pyramid would
//! use; the GL_LINEAR filtering of 16f textures is not exercised here (it is
//! supported by the same extension set, verified via the read-only glxinfo
//! extension evidence recorded in the task log). All
//! metric numbers are printed with `eprintln!`; the assertions enforce the
//! gate's *premises*: the current 8-bit path deviates measurably from the
//! linear-light reference on high-contrast scenes (a visible dark halo) and
//! is indistinguishable from it on gradients, while a half-float linear-light
//! intermediate removes the halo. No assertion enforces a subjective quality
//! bar.

use smithay::backend::allocator::Fourcc;
use smithay::backend::egl::native::EGLSurfacelessDisplay;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{
    Bind as _, Color32F, ExportMem as _, Offscreen as _, Texture as _,
};
use smithay::utils::{Point, Scale, Size, Transform};

use crate::render_helpers::blur::{
    retained_blur_texture_bytes, Blur, BlurOptions, BlurTrace,
    BLUR_BUDGET_TEST_LOCK as BLUR_BUDGET_LOCK,
};
use crate::render_helpers::render_to_texture;
use crate::render_helpers::renderer::ScratchFramebuffer;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::utils::lifecycle_diag;

/// Run inside the serialized lifecycle-diag window so counters are exact.
fn with_diag<R>(f: impl FnOnce() -> R) -> R {
    lifecycle_diag::with_enabled_for_test(f)
}

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

// ---------------------------------------------------------------------------
// Color-space helpers
// ---------------------------------------------------------------------------

/// sRGB (nonlinear) -> linear-light conversion of an 8-bit code value.
fn srgb_u8_to_linear(code: u8) -> f64 {
    let c = f64::from(code) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear-light -> sRGB 8-bit code value (round-to-nearest).
fn linear_to_srgb_u8(v: f64) -> u8 {
    let c = v.clamp(0.0, 1.0);
    let s = if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (s.clamp(0.0, 1.0) * 255.0 + 0.5).floor() as u8
}

/// Encode a linear value into half precision (IEEE 754 binary16 with
/// round-to-nearest-even, matching the GPU's RGBA16F store). ~10-bit
/// mantissa, so the quantization at 0.5 is ~2^-11 (~0.13 of an 8-bit code).
fn f16_encode(v: f64) -> f64 {
    f16_bits_to_f64(f64_to_f16_bits(v))
}

fn f64_to_f16_bits(v: f64) -> u16 {
    if v == 0.0 {
        return if v.is_sign_negative() { 0x8000 } else { 0 };
    }
    if v.is_nan() {
        // Quiet NaN with the sign preserved.
        return 0x7e00 | if v.is_sign_negative() { 0x8000 } else { 0 };
    }
    if v.is_infinite() {
        return 0x7c00 | if v.is_sign_negative() { 0x8000 } else { 0 };
    }
    let bits = v.to_bits();
    let sign = ((bits >> 63) & 1) as u16;
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let mant = bits & 0x000f_ffff_ffff_ffff;

    let e16 = exp - 1023 + 15;
    if e16 >= 31 {
        return (sign << 15) | 0x7c00; // overflow -> inf
    }
    if e16 <= 0 {
        // Subnormal or zero. The 53-bit significand (implicit 1 included)
        // is shifted down by 43 - e16 to land in f16 subnormal units
        // (1 unit = 2^-24); the shifted-out bits drive round-to-nearest-even.
        if e16 < -10 {
            return sign << 15; // underflow -> zero
        }
        let m = mant | 0x0010_0000_0000_0000;
        let shift = 43 - e16;
        let half = (m >> shift) as u16;
        let round = ((m >> (shift - 1)) & 1) as u16;
        let sticky = (m & ((1u64 << (shift - 1)) - 1)) != 0;
        let mut out = (sign << 15) | half;
        if round == 1 && (sticky || (half & 1) == 1) {
            out = out.wrapping_add(1); // may carry into the exponent -> min normal
        }
        return out;
    }
    let half_mant = (mant >> 42) as u16; // 52 - 10
    let round = ((mant >> 41) & 1) as u16;
    let sticky = (mant & ((1u64 << 41) - 1)) != 0;
    let mut out = (sign << 15) | ((e16 as u16) << 10) | half_mant;
    if round == 1 && (sticky || (half_mant & 1) == 1) {
        out = out.wrapping_add(1); // may carry into the exponent -> inf at e16=30
    }
    out
}

fn f16_bits_to_f64(bits: u16) -> f64 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exponent = ((bits >> 10) & 0x1f) as i32;
    let mantissa = f64::from(bits & 0x3ff);
    if exponent == 0 {
        if mantissa == 0.0 {
            return sign * 0.0;
        }
        return sign * mantissa * (2.0f64).powi(-24); // subnormal
    }
    if exponent == 31 {
        return if mantissa == 0.0 {
            sign * f64::INFINITY
        } else {
            f64::NAN
        };
    }
    sign * (1.0 + mantissa / 1024.0) * (2.0f64).powi(exponent - 15)
}

/// The f16 encoder is a measurement instrument for the A13.1 half-float
/// candidate. Its special-case branches (subnormal, round-to-nearest-even
/// ties, overflow, NaN) never trigger inside the three fixed scenes (their
/// linear values stay >= 2^-14, where the blur weights keep the value range
/// bounded), so without a direct unit test a regression of those branches
/// would go unnoticed — the first review round found exactly such a
/// regression (subnormal shift 14-e16 instead of 43-e16). Expected bit
/// patterns were independently verified against IEEE 754 binary16
/// (`struct.pack('<e')`).
#[test]
fn f16_encoder_special_cases_match_ieee_754() {
    let cases: &[(f64, u16)] = &[
        (0.0, 0x0000),
        (-0.0, 0x8000),
        (1.0, 0x3c00),
        (-1.0, 0xbc00),
        (2.0f64.powi(-20), 0x0010),       // subnormal
        (6.0e-5, 0x03ef),                 // subnormal, RNE
        (1.0 + 2.0f64.powi(-11), 0x3c00), // tie -> even
        (1.5 + 2.0f64.powi(-11), 0x3e00), // tie -> even
        (2.0f64.powi(-25), 0x0000),       // underflow -> zero
        (2.0f64.powi(-24), 0x0001),       // min subnormal
        (65504.0, 0x7bff),                // max finite
        (65520.0, 0x7c00),                // overflow -> inf
        (f64::INFINITY, 0x7c00),
        (f64::NEG_INFINITY, 0xfc00),
        (f64::NAN, 0x7e00), // quiet NaN, sign preserved
    ];
    for (value, expected) in cases {
        assert_eq!(
            f64_to_f16_bits(*value),
            *expected,
            "f16 encoding of {value} must be 0x{expected:04x}"
        );
    }
}

// ---------------------------------------------------------------------------
// CPU mirror of the blur pipeline
// ---------------------------------------------------------------------------

/// GL_LINEAR sample of `src` at pixel coordinates (px, py), with the exact
/// GLES semantics of the blur shaders: the coordinate is in texel units with
/// texel centers at integer + 0.5, so the interpolation taps are
/// `i0 = floor(u - 0.5)` with fraction `u - 0.5 - i0`. The `tex_bounds`
/// clamp of blur_down/blur_up (plus CLAMP_TO_EDGE) bounds the coordinate to
/// [0.5, w - 0.5], i.e. the first/last texel centers.
fn linear_sample(src: &[f64], w: usize, h: usize, px: f64, py: f64) -> f64 {
    let u = px.clamp(0.5, w as f64 - 0.5);
    let v = py.clamp(0.5, h as f64 - 0.5);
    let i0 = (u - 0.5).floor() as usize;
    let j0 = (v - 0.5).floor() as usize;
    let i1 = (i0 + 1).min(w - 1);
    let j1 = (j0 + 1).min(h - 1);
    let tx = u - 0.5 - i0 as f64;
    let ty = v - 0.5 - j0 as f64;
    let a = src[j0 * w + i0] * (1.0 - tx) + src[j0 * w + i1] * tx;
    let b = src[j1 * w + i0] * (1.0 - tx) + src[j1 * w + i1] * tx;
    a * (1.0 - ty) + b * ty
}

/// One dual-Kawase pass exactly as `blur_down.frag` / `blur_up.frag` sample
/// (offset=4, tex_scale=1, no capacity slack):
///
/// - down: output pixel center maps to source pixel 2x+1, the four arms land at +-4 source pixels,
///   weights 4/8 center + 1/8 each arm;
/// - up: output pixel center maps to source coordinate x/2+0.25 (sub-pixel, interpolated), four
///   edges at +-4 px and four corners at +-2 px (double weight), no center tap, /12.
fn dual_kawase_pass(src: &[f64], src_w: usize, src_h: usize, up: bool) -> Vec<f64> {
    let (dst_w, dst_h) = if up {
        (src_w * 2, src_h * 2)
    } else {
        (src_w / 2, src_h / 2)
    };
    let mut dst = vec![0.0f64; dst_w * dst_h];
    let sample = |px: f64, py: f64| linear_sample(src, src_w, src_h, px, py);
    for y in 0..dst_h {
        for x in 0..dst_w {
            let (cx, cy) = if up {
                (x as f64 / 2.0 + 0.25, y as f64 / 2.0 + 0.25)
            } else {
                ((2 * x + 1) as f64, (2 * y + 1) as f64)
            };
            let value = if up {
                let mut sum = sample(cx + 4.0, cy)
                    + sample(cx - 4.0, cy)
                    + sample(cx, cy + 4.0)
                    + sample(cx, cy - 4.0);
                sum += 2.0
                    * (sample(cx + 2.0, cy + 2.0)
                        + sample(cx - 2.0, cy + 2.0)
                        + sample(cx + 2.0, cy - 2.0)
                        + sample(cx - 2.0, cy - 2.0));
                sum / 12.0
            } else {
                let mut sum = 4.0 * sample(cx, cy);
                sum += sample(cx + 4.0, cy) + sample(cx - 4.0, cy);
                sum += sample(cx, cy + 4.0) + sample(cx, cy - 4.0);
                sum / 8.0
            };
            dst[y * dst_w + x] = value;
        }
    }
    dst
}

/// Run the full 3-pass dual-Kawase pyramid (512 -> 256 -> 128 -> 64 ->
/// 128 -> 256 -> 512), applying `quantize` after every pyramid store exactly
/// like the 8-bit (or 16f) textures do.
fn pyramid_3pass(src: &[f64], w: usize, h: usize, quantize: impl Fn(f64) -> f64) -> Vec<f64> {
    let mut cur = src.to_vec();
    let mut cur_w = w;
    let mut cur_h = h;
    for _ in 0..3 {
        cur = dual_kawase_pass(&cur, cur_w, cur_h, false);
        cur_w /= 2;
        cur_h /= 2;
        for v in cur.iter_mut() {
            *v = quantize(*v);
        }
    }
    for _ in 0..3 {
        cur = dual_kawase_pass(&cur, cur_w, cur_h, true);
        cur_w *= 2;
        cur_h *= 2;
        for v in cur.iter_mut() {
            *v = quantize(*v);
        }
    }
    cur
}

/// The current production path: the shaders average *sRGB code values*
/// directly (no linear-light decode), every pyramid level is an 8-bit
/// texture, and the final output lands in an 8-bit framebuffer. Returns the
/// blurred values in linear-light space for comparison.
fn pipeline_8bit_nonlinear(codes: &[u8], w: usize, h: usize) -> Vec<u8> {
    let srgb: Vec<f64> = codes.iter().map(|c| f64::from(*c) / 255.0).collect();
    // 8-bit store: round the sRGB-space value to a code, then back to [0,1].
    let quantize_u8 = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() / 255.0;
    let out = pyramid_3pass(&srgb, w, h, quantize_u8);
    out.iter().map(|v| (v * 255.0).round() as u8).collect()
}

/// The linear-light f64 reference: codes decoded to linear, blurred with no
/// intermediate quantization, final 8-bit encode. Returns sRGB codes.
fn pipeline_linear_reference(codes: &[u8], w: usize, h: usize) -> Vec<u8> {
    let linear: Vec<f64> = codes.iter().map(|c| srgb_u8_to_linear(*c)).collect();
    let out = pyramid_3pass(&linear, w, h, |v| v);
    out.iter().map(|v| linear_to_srgb_u8(*v)).collect()
}

/// The half-float linear-light candidate: linear decode, RGBA16F quantization
/// after every pyramid store, final 8-bit encode. Returns sRGB codes.
fn pipeline_16f_linear(codes: &[u8], w: usize, h: usize) -> Vec<u8> {
    let linear: Vec<f64> = codes.iter().map(|c| srgb_u8_to_linear(*c)).collect();
    let out = pyramid_3pass(&linear, w, h, f16_encode);
    out.iter().map(|v| linear_to_srgb_u8(*v)).collect()
}

// ---------------------------------------------------------------------------
// Scenes and metrics
// ---------------------------------------------------------------------------

/// 10-bit gradient scene: linear values spanning [0.02, 0.98] with 1024
/// intermediate steps, so adjacent 8-bit codes differ by well under one code
/// — exactly the condition that exposes banding.
fn ten_bit_gradient(w: usize, h: usize) -> Vec<u8> {
    let mut codes = Vec::with_capacity(w * h);
    for _ in 0..h {
        for x in 0..w {
            let v = 0.02 + 0.96 * x as f64 / (w.saturating_sub(1).max(1) as f64);
            codes.push(linear_to_srgb_u8(v));
        }
    }
    codes
}

/// High-contrast edge scene: linear 0.05 to 0.95 over 2 px at the middle
/// column, constant along y.
fn high_contrast_edge(w: usize, h: usize) -> Vec<u8> {
    let edge_at = w / 2;
    let mut codes = Vec::with_capacity(w * h);
    for _ in 0..h {
        for x in 0..w {
            let t = ((x as f64 - edge_at as f64) / 2.0).clamp(0.0, 1.0);
            codes.push(linear_to_srgb_u8(0.05 + 0.90 * t));
        }
    }
    codes
}

/// Wallpaper proxy scene: diagonal gradient + high-contrast edge + fixed
/// pseudo-noise, so it behaves like a real wallpaper under a glass panel.
fn wallpaper_proxy(w: usize, h: usize) -> Vec<u8> {
    let mut codes = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let edge = ((x as f64 - (w * 3 / 4) as f64) / 2.0).clamp(0.0, 1.0);
            let diag = (x as f64 / w.max(1) as f64 * 0.5 + y as f64 / h.max(1) as f64 * 0.5)
                .clamp(0.0, 1.0);
            let noise = (x.wrapping_mul(2654435761usize) ^ y.wrapping_mul(2246822519usize)) & 0xff;
            let noise = noise as f64 / 255.0;
            codes.push(linear_to_srgb_u8(
                (edge * 0.9 + diag * 0.4 + noise * 0.1).clamp(0.0, 1.0),
            ));
        }
    }
    codes
}

/// Metrics in *sRGB code units* (0..255), computed on the row-interior
/// neighbor pairs only — the row boundary (last pixel of row y, first pixel
/// of row y+1) is a scene discontinuity, not a blur banding signal.
#[derive(Clone, Copy, Debug, Default)]
struct BandingMetrics {
    /// Number of row-interior adjacent pixel pairs whose code difference
    /// exceeds one code: a visible step on a gradient.
    step_pairs: usize,
    /// Largest row-interior adjacent code difference.
    max_step: f64,
    /// Mean absolute code error vs the reference.
    mean_abs_err: f64,
    /// Largest absolute code error vs the reference.
    max_abs_err: f64,
}

fn banding_metrics(blurred: &[u8], reference: &[u8], w: usize) -> BandingMetrics {
    let mut m = BandingMetrics::default();
    let mut sum: f64 = 0.0;
    let mut max_err: f64 = 0.0;
    for (i, (b, r)) in blurred.iter().zip(reference.iter()).enumerate() {
        let err = f64::from(i32::from(*b) - i32::from(*r)).abs();
        sum += err;
        max_err = max_err.max(err);
        // Row-interior pairs only: x in 1..w for each row.
        if i % w != 0 {
            let step = f64::from(i32::from(blurred[i - 1]) - i32::from(*b)).abs();
            if step > 1.0 {
                m.step_pairs += 1;
            }
            m.max_step = m.max_step.max(step);
        }
    }
    m.mean_abs_err = sum / blurred.len() as f64;
    m.max_abs_err = max_err;
    m
}

/// Measure the three pipelines on one scene and return the 8-bit and 16f
/// metrics in sRGB code units (the reference is the linear-light f64 blur
/// re-encoded to 8-bit codes).
fn measure_scene(
    scene: &str,
    codes: &[u8],
    w: usize,
    h: usize,
) -> (BandingMetrics, BandingMetrics) {
    let reference = pipeline_linear_reference(codes, w, h);
    let legacy = pipeline_8bit_nonlinear(codes, w, h);
    let half = pipeline_16f_linear(codes, w, h);
    let m8 = banding_metrics(&legacy, &reference, w);
    let m16 = banding_metrics(&half, &reference, w);
    eprintln!(
        "[t13] {scene}: 8bit steps>1={} max_step={:.1} mean_err={:.3} max_err={} (sRGB codes)",
        m8.step_pairs, m8.max_step, m8.mean_abs_err, m8.max_abs_err
    );
    eprintln!(
        "[t13] {scene}: 16f  steps>1={} max_step={:.1} mean_err={:.3} max_err={} (sRGB codes)",
        m16.step_pairs, m16.max_step, m16.mean_abs_err, m16.max_abs_err
    );
    (m8, m16)
}

// ---------------------------------------------------------------------------
// A13.1 evidence tests
// ---------------------------------------------------------------------------

/// Gradient scene: the premise of the PROPOSAL was that 8-bit *nonlinear*
/// blur banded smooth gradients. The measurement shows the opposite: with
/// 8-bit-encoded input and output, the blur-space quantization error stays
/// small (peak ~3 codes) and is comparable to the half-float path (peak
/// ~1-3 codes) — the input/output 8-bit encoding dominates, so a half-float
/// intermediate buys almost nothing on gradients. This assertion records the
/// refutation: the 8-bit gradient error stays below a quarter of the peak
/// edge error (~35 codes), and 16f does not beat 8-bit meaningfully.
#[test]
fn gradient_banding_8bit_vs_linear_and_16f() {
    let (w, h) = (512, 64);
    let (m8, m16) = measure_scene("gradient", &ten_bit_gradient(w, h), w, h);
    assert!(
        m8.max_abs_err < 10.0,
        "gradient banding from blur-space quantization must stay small \
         (refutes the PROPOSAL premise), got max_err={}",
        m8.max_abs_err
    );
    assert!(
        m16.max_abs_err <= m8.max_abs_err + 1.0,
        "16f must not be worse than 8-bit on gradients: {} vs {}",
        m16.max_abs_err,
        m8.max_abs_err
    );
}

/// High-contrast edge scene: the dark-halo premise. Blurring sRGB code
/// values directly puts the edge transition in the wrong place vs the linear
/// reference; the peak deviation is ~35 codes (about 13% of the 0..255
/// range, a clearly visible dark halo) and a half-float linear-light pyramid
/// cuts it to ~1 code. This is the *confirmed* part of the PROPOSAL, and it
/// quantifies the A13.1 "fixed scene" evidence.
#[test]
fn high_contrast_edge_halo_quantified() {
    let (w, h) = (256, 64);
    let (m8, m16) = measure_scene("edge", &high_contrast_edge(w, h), w, h);
    assert!(
        m8.max_abs_err > 20.0,
        "the 8-bit nonlinear edge blur must deviate measurably from the linear \
         reference (the halo premise), got max_err={}",
        m8.max_abs_err
    );
    assert!(
        m16.max_abs_err < m8.max_abs_err / 4.0,
        "half-float linear-light must cut the edge halo at least 4x, got \
         {} vs {}",
        m16.max_abs_err,
        m8.max_abs_err
    );
}

/// Wallpaper proxy scene: gradient + edge + noise, the realistic-scene
/// counterpart of the synthetic scenes above. The 8-bit peak error (~12
/// codes) is smaller than the pure edge scene but still clearly above the
/// half-float path (~1 code).
#[test]
fn wallpaper_proxy_scene_error_quantified() {
    let (w, h) = (512, 64);
    let (m8, m16) = measure_scene("wallpaper", &wallpaper_proxy(w, h), w, h);
    assert!(
        m8.max_abs_err > 5.0,
        "the 8-bit wallpaper blur must deviate measurably from the linear \
         reference, got max_err={}",
        m8.max_abs_err
    );
    assert!(
        m16.max_abs_err < m8.max_abs_err / 4.0,
        "half-float linear-light must cut the wallpaper error at least 4x, got \
         {} vs {}",
        m16.max_abs_err,
        m8.max_abs_err
    );
}

// ---------------------------------------------------------------------------
// A13.2 single-pass contract
// ---------------------------------------------------------------------------

/// A13.2: the decode -> blur -> postprocess -> encode chain runs exactly once
/// per region per frame. Through the real GL pipeline, one `Blur::render`
/// must account exactly one blur run and one pyramid (4 allocations on the
/// first prepare; a second render of the same pyramid reuses it — `reuses`
/// would count 1). This test measures the first prepare/render pair; a
/// second conversion anywhere in the chain would show up as an extra blur
/// run or allocation. The capture side of the chain (one
/// `capture_framebuffer` per region) is already asserted by the T11/T12 gate
/// tests on the real `Niri::render` path, and T12's G4 scans the render
/// helpers for a second capture authority.
#[test]
fn single_blur_run_and_single_allocation_per_render() {
    let _lock = BLUR_BUDGET_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut renderer = make_renderer();
    let mut blur = Blur::new(&mut renderer).expect("blur program");
    let source = renderer
        .create_buffer(Fourcc::Abgr8888, Size::from((256, 160)))
        .expect("creating the blur source texture");

    let (blur_runs, allocations, reuses) = with_diag(|| {
        blur.prepare_textures(
            |fourcc, size| renderer.create_buffer(fourcc, size),
            &source,
            options(3),
            BlurTrace::default(),
        )
        .expect("preparing blur textures");
        blur.render(&mut renderer, &source, options(3))
            .expect("rendering blur");
        let s = lifecycle_diag::snapshot();
        (
            s.blur_render,
            s.blur_texture_allocations,
            s.blur_texture_reuses,
        )
    });

    eprintln!(
        "[t13] A13.2: one render -> blur_runs={blur_runs}, allocations={allocations}, reuses={reuses}"
    );
    assert_eq!(
        blur_runs, 1,
        "one Blur::render must run the pyramid exactly once (no second conversion)"
    );
    assert_eq!(
        allocations, 4,
        "the pyramid prepares once (initial allocation of its 4 levels)"
    );
    assert_eq!(reuses, 0, "the first prepare cannot reuse anything");
}

// ---------------------------------------------------------------------------
// A13.3 capability evidence
// ---------------------------------------------------------------------------

/// Render a solid color into an RGBA16F texture through the real GL pipeline
/// (create -> bind -> draw -> readback) and return the f16 pixels. On
/// renderers without half-float-renderable support this returns an error —
/// which is exactly the evidence the fallback gate needs (the GO path only
/// exists where this succeeds).
fn render_and_read_16f(
    renderer: &mut GlesRenderer,
    size: Size<i32, smithay::utils::Physical>,
    color: Color32F,
) -> Result<Vec<f32>, String> {
    let buffer = SolidColorBuffer::new((f64::from(size.w), f64::from(size.h)), color);
    let element =
        SolidColorRenderElement::from_buffer(&buffer, Point::from((0., 0.)), 1., Kind::Unspecified);
    let (texture, _sync) = render_to_texture(
        renderer,
        size,
        Scale::from(1.),
        Transform::Normal,
        Fourcc::Abgr16161616f,
        std::iter::once(element),
    )
    .map_err(|e| format!("RGBA16F render failed: {e}"))?;

    let bytes = read_texture_f16(renderer, &texture)
        .map_err(|e| format!("RGBA16F readback failed: {e}"))?;
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        let raw = u16::from_le_bytes([chunk[0], chunk[1]]);
        out.push(f16_bits_to_f64(raw) as f32);
    }
    Ok(out)
}

/// Read the full RGBA16F content of a texture with a direct glReadPixels
/// (RGBA/HALF_FLOAT), bypassing smithay's `map_texture` — which hardcodes
/// 4 bytes per pixel and would truncate a 16f readback to half its size.
fn read_texture_f16(
    renderer: &mut GlesRenderer,
    texture: &smithay::backend::renderer::gles::GlesTexture,
) -> anyhow::Result<Vec<u8>> {
    let size = texture.size();
    let mut out = vec![0u8; (size.w as usize * size.h as usize * 8) as usize];
    let scratch = ScratchFramebuffer::for_renderer(renderer);
    let mut gl_error = false;
    renderer.with_context(|gl| unsafe {
        let mut prev_fb = 0;
        gl.GetIntegerv(
            smithay::backend::renderer::gles::ffi::FRAMEBUFFER_BINDING,
            &mut prev_fb,
        );
        let mut prev_viewport = [0; 4];
        gl.GetIntegerv(
            smithay::backend::renderer::gles::ffi::VIEWPORT,
            prev_viewport.as_mut_ptr(),
        );
        let fb = scratch.get_or_create(gl);
        gl.BindFramebuffer(smithay::backend::renderer::gles::ffi::FRAMEBUFFER, fb);
        gl.FramebufferTexture2D(
            smithay::backend::renderer::gles::ffi::FRAMEBUFFER,
            smithay::backend::renderer::gles::ffi::COLOR_ATTACHMENT0,
            smithay::backend::renderer::gles::ffi::TEXTURE_2D,
            texture.tex_id(),
            0,
        );
        gl.Viewport(0, 0, size.w, size.h);
        gl.ReadPixels(
            0,
            0,
            size.w,
            size.h,
            smithay::backend::renderer::gles::ffi::RGBA,
            smithay::backend::renderer::gles::ffi::HALF_FLOAT,
            out.as_mut_ptr().cast(),
        );
        gl.FramebufferTexture2D(
            smithay::backend::renderer::gles::ffi::FRAMEBUFFER,
            smithay::backend::renderer::gles::ffi::COLOR_ATTACHMENT0,
            smithay::backend::renderer::gles::ffi::TEXTURE_2D,
            0,
            0,
        );
        gl.BindFramebuffer(
            smithay::backend::renderer::gles::ffi::FRAMEBUFFER,
            prev_fb as u32,
        );
        gl.Viewport(
            prev_viewport[0],
            prev_viewport[1],
            prev_viewport[2],
            prev_viewport[3],
        );
        gl_error = gl.GetError() != smithay::backend::renderer::gles::ffi::NO_ERROR;
    })?; // the outer `?` propagates make-current errors
    if gl_error {
        return Err(anyhow::anyhow!("GL error after RGBA16F readback"));
    }
    Ok(out)
}

/// Read the current content of an 8-bit texture back to RGBA bytes (the
/// blur_capacity helper pattern).
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

/// A13.3 capability probe on the target GPU: create, render to and read back
/// an RGBA16F texture through the real GL pipeline. On renderers without
/// half-float renderability this fails and the failure is recorded as the
/// fallback evidence (the GO format path would be unavailable there); on
/// this machine the readback values must come back correct (asserted below).
#[test]
fn half_float_renderable_capability_probe() {
    let mut renderer = make_renderer();
    let size = Size::from((64, 64));
    match render_and_read_16f(&mut renderer, size, Color32F::new(0.5, 0.25, 0.75, 1.0)) {
        Ok(pixels) => {
            assert_eq!(
                pixels.len(),
                64 * 64 * 4,
                "RGBA16F readback must be 4 f16 per pixel"
            );
            let sample = &pixels[((32 * 64 + 32) * 4)..][..4];
            eprintln!("[t13] A13.3: RGBA16F renderable=YES sample={sample:?}");
            let expect = [0.5, 0.25, 0.75, 1.0];
            for (i, want) in expect.iter().enumerate() {
                let got = f64::from(sample[i]);
                assert!(
                    (got - want).abs() < 0.01,
                    "RGBA16F roundtrip mismatch: got {got}, want {want}"
                );
            }
        }
        Err(err) => {
            eprintln!("[t13] A13.3: RGBA16F renderable=NO — {err}");
            eprintln!(
                "[t13] (evidence: the half-float path is unavailable on this renderer; \
                       the GO format path would have to fall back here)"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// GPU vs CPU-mirror calibration
// ---------------------------------------------------------------------------

/// Run the production dual-Kawase pipeline (`Blur::render`, 8-bit pyramid)
/// on a solid source and read the output back. On a uniform color the blur
/// is a no-op in any space where the color is a fixed point of the encoding
/// roundtrip (code 128 is such a point: sRGB encode(linear decode(128/255))
/// = 128), so this test verifies that the GPU path, the CPU mirror and the
/// readback agree on the *8-bit sRGB code-space semantics* of the pipeline:
/// the GL texture stores raw codes (no sRGB framebuffer conversion), the
/// shaders average codes directly, and an 8-bit uniform passes through
/// unchanged. It does NOT discriminate between code-space averaging and a
/// correct linear-light pipeline (a uniform field is a fixed point of both);
/// the gamma model of the mirror is validated by the fact that the real
/// shaders contain no gamma functions and the edge-scene mirror numbers are
/// reproduced by independent re-derivation.
#[test]
fn gpu_blur_uniform_color_matches_cpu_mirror() {
    let _lock = BLUR_BUDGET_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut renderer = make_renderer();
    let mut blur = Blur::new(&mut renderer).expect("blur program");

    // Source code 128: the GL framebuffer stores raw [0,1] values as 8-bit
    // codes (no sRGB framebuffer conversion), so passing 128/255 produces a
    // texture whose code is exactly 128.
    let code_128 = 128.0 / 255.0;
    let color = Color32F::new(code_128, code_128, code_128, 1.0);
    let buffer = SolidColorBuffer::new((256., 256.), color);
    let element =
        SolidColorRenderElement::from_buffer(&buffer, Point::from((0., 0.)), 1., Kind::Unspecified);
    let (source, _sync) = render_to_texture(
        &mut renderer,
        Size::from((256, 256)),
        Scale::from(1.),
        Transform::Normal,
        Fourcc::Abgr8888,
        std::iter::once(element),
    )
    .expect("rendering the solid source");

    // Run the blur inside the diag window so the GPU work is serialized with
    // the other counting tests (a concurrent renderer would contaminate
    // their deltas).
    let output = with_diag(|| {
        blur.prepare_textures(
            |fourcc, size| renderer.create_buffer(fourcc, size),
            &source,
            options(3),
            BlurTrace::default(),
        )
        .expect("preparing blur textures");
        blur.render(&mut renderer, &source, options(3))
            .expect("rendering blur")
    });
    let pixels = read_texture(&mut renderer, &output.texture);
    let px = &pixels[(128 * 256 + 128) * 4..][..4];

    let got = f64::from(px[0]);
    eprintln!(
        "[t13] GPU blur uniform: source code=128, output code={got} (linear {:.4})",
        srgb_u8_to_linear(px[0])
    );
    assert!(
        (got - 128.0).abs() <= 1.5,
        "GPU blur of a uniform color must be a no-op in code space: {got}"
    );

    // The CPU mirror predicts the same value: 3 down + 3 up passes of a
    // constant field stay constant (weights sum to 1 in both kernels).
    let codes = vec![128u8; 256 * 256];
    let mirrored = pipeline_8bit_nonlinear(&codes, 256, 256);
    let mirrored_center = mirrored[128 * 256 + 128];
    let expected_code = f64::from(mirrored_center);
    eprintln!("[t13] CPU mirror uniform: predicted code={expected_code}");
    assert!(
        (got - expected_code).abs() <= 1.5,
        "GPU blur output ({got}) must match the CPU mirror prediction ({expected_code})"
    );
    let bytes = retained_blur_texture_bytes();
    assert!(bytes > 0, "the blur pyramid must charge the global budget");
}

// ---------------------------------------------------------------------------
// A13.4 budget baseline
// ---------------------------------------------------------------------------

/// A13.4: record the memory delta of the proposed half-float pyramid (an
/// RGBA16F pyramid is exactly 2x the 8-bit bytes — the bytes-per-pixel is
/// the only difference, the level sizes are identical), so the 4K
/// multi-region budget comparison at GO time is anchored to a deterministic
/// number. This is arithmetic over the pyramid level sizes (8 B/px vs
/// 4 B/px, exactly what `texture_bytes` would charge for a 16f pyramid);
/// the current production path only ever allocates 8-bit pyramids, so no GL
/// measurement exists for the 16f case. Frame-time and power deltas are not
/// measured here (no GO, no obligation — recorded honestly).
#[test]
fn half_float_pyramid_memory_delta_baseline() {
    let (w, h) = (4096i32, 2160i32);
    let px = w as usize * h as usize;
    let rgba8_bytes = px * 4;
    let f16_bytes = px * 8;
    let passes = 3usize;
    let pyramid_8: usize = (0..=passes)
        .map(|i| (w as usize >> i) * (h as usize >> i) * 4)
        .sum();
    let pyramid_16: usize = (0..=passes)
        .map(|i| (w as usize >> i) * (h as usize >> i) * 8)
        .sum();
    eprintln!(
        "[t13] A13.4: 4K pyramid bytes 8bit={pyramid_8} 16f={pyramid_16} \
         (per-level {rgba8_bytes}B vs {f16_bytes}B) delta={}",
        pyramid_16 - pyramid_8
    );
    assert_eq!(
        pyramid_16,
        pyramid_8 * 2,
        "16f pyramid is exactly 2x the 8-bit bytes"
    );
    assert!(
        pyramid_16 < 256 * 1024 * 1024,
        "the 16f 4K pyramid must fit the per-pyramid hard budget (256 MiB)"
    );
}
