use std::sync::{Arc, Mutex};

use niri_config::CornerRadius;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Point, Rectangle, Scale};
use smithay::wayland::compositor::{with_states, SurfaceData};
use wayland_server::protocol::wl_surface::WlSurface;
use wayland_server::Resource as _;

use crate::handlers::background_effect::get_cached_blur_region;
use crate::niri_render_elements;
use crate::render_helpers::blur::BlurTrace;
use crate::render_helpers::damage::ExtraDamage;
use crate::render_helpers::framebuffer_effect::{FramebufferEffect, FramebufferEffectElement};
use crate::render_helpers::resolved_effect_plan::{
    ResolvedEffectCaptureKey, ResolvedEffectPlan, ResolvedEffectVisualKey,
};
use crate::render_helpers::xray::{XrayElement, XrayPos};
use crate::render_helpers::{blur, RenderCtx};
use crate::utils::region::TransformedRegion;
use crate::utils::surface_geo;

#[derive(Debug)]
pub struct BackgroundEffect {
    nonxray: FramebufferEffect,
    /// Damage when resolved plan visuals change.
    ///
    /// On the live path this element is pushed directly above the framebuffer
    /// effect, so draw-only material changes repaint the effect without
    /// bumping its commit — the cached blurred texture is reused (P03).
    damage: ExtraDamage,
    /// Last resolved visual fingerprint (R12). Geometry is not included;
    /// only material/blur/radius changes need ExtraDamage on the effect.
    last_visual: Option<ResolvedEffectVisualKey>,
    /// Last resolved capture fingerprint (P03). Only changes here bump the
    /// live effect commit and force a re-blit + blur pyramid re-run.
    last_capture: Option<ResolvedEffectCaptureKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Options {
    pub blur: bool,
    pub xray: bool,
    pub noise: Option<f64>,
    pub saturation: Option<f64>,
    pub glass: GlassOptions,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlassOptions {
    pub tint_color: [f32; 4],
    pub tint_amount: f32,
    pub contrast: f32,
    pub edge_highlight: f32,
    pub refraction: f32,
    pub inner_shadow: f32,
    pub chromatic: f32,
    pub lens_depth: f32,
}

impl Default for GlassOptions {
    fn default() -> Self {
        Self {
            tint_color: [1., 1., 1., 1.],
            tint_amount: 0.,
            contrast: 1.,
            edge_highlight: 0.,
            refraction: 0.,
            inner_shadow: 0.,
            chromatic: 0.,
            lens_depth: 0.,
        }
    }
}

impl GlassOptions {
    pub(crate) fn from_effect(effect: niri_config::BackgroundEffect) -> Self {
        let tint_color = effect
            .tint_color
            .unwrap_or_else(|| niri_config::Color::new_unpremul(1., 1., 1., 1.))
            .to_array_unpremul();

        Self {
            tint_color,
            tint_amount: effect.tint_amount.unwrap_or(0.) as f32,
            contrast: effect.contrast.unwrap_or(1.) as f32,
            edge_highlight: effect.edge_highlight.unwrap_or(0.) as f32,
            refraction: effect.refraction.unwrap_or(0.) as f32,
            inner_shadow: effect.inner_shadow.unwrap_or(0.) as f32,
            chromatic: effect.chromatic.unwrap_or(0.) as f32,
            lens_depth: effect.lens_depth.unwrap_or(0.) as f32,
        }
    }

    pub(crate) fn is_visible(&self) -> bool {
        self.tint_amount > 0.
            || self.contrast != 1.
            || self.edge_highlight > 0.
            || self.refraction > 0.
            || self.inner_shadow > 0.
            || self.chromatic > 0.
            || self.lens_depth > 0.
    }
}

impl Options {
    pub(crate) fn is_visible(&self) -> bool {
        self.xray
            || self.blur
            || self.noise.is_some_and(|x| x > 0.)
            || self.saturation.is_some_and(|x| x != 1.)
            || self.glass.is_visible()
    }
}

/// Render-time parameters.
#[derive(Debug, Clone)]
pub struct RenderParams {
    /// Geometry of the background effect.
    pub geometry: Rectangle<f64, Logical>,
    /// Final opacity for the rendered effect.
    pub alpha: f32,
    /// Effect subregion, will be clipped to `geometry`.
    ///
    /// `subregion.iter()` should return `geometry`-relative rectangles.
    pub subregion: Option<TransformedRegion>,
    /// Geometry and radius for clipping in the same coordinate space as `geometry`.
    pub clip: Option<(Rectangle<f64, Logical>, CornerRadius)>,
    /// Scale to use for rounding to physical pixels.
    pub scale: f64,
    /// Additional physical-space clip applied only while drawing.
    ///
    /// Unlike wrapping the render element in `CropRenderElement`, this keeps
    /// framebuffer capture on the full effect geometry. This is needed by
    /// edge-reveal animations: Tahoe glass samples outside its visible panel
    /// bounds for blur/refraction padding, while the panel itself must remain
    /// clipped to the reveal edge.
    pub draw_clip: Option<Rectangle<i32, smithay::utils::Physical>>,
    /// T-32 R-3a: the physical capture band actually blitted this frame.
    ///
    /// `None` = the precise rounded geometry (pre-quantization behavior).
    /// `Some(band)` = an outward-expanded 8px-grid superset used while geometry
    /// is animating; the capture key, the blit source and the draw-side
    /// sub-rectangle all agree on this band.
    pub capture_band: Option<Rectangle<i32, smithay::utils::Physical>>,
}

/// Geometry to use when the client supplied an explicit blur region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientBlurRegionGeometry {
    /// Keep the historical behavior: render the effect across the whole surface geometry.
    Surface,
    /// Use the bounding box of the client blur region as both effect and clip geometry.
    BoundingBox,
}

impl RenderParams {
    pub(crate) fn fit_clip_radius(&mut self) {
        if let Some((geo, radius)) = &mut self.clip {
            // HACK: increase radius to avoid slight bleed on rounded corners.
            *radius = radius.expanded_by(1.);

            *radius = radius.fit_to(geo.size.w as f32, geo.size.h as f32);
        }
    }
}

niri_render_elements! {
    BackgroundEffectElement => {
        FramebufferEffect = FramebufferEffectElement,
        Xray = XrayElement,
        ExtraDamage = ExtraDamage,
    }
}

impl BackgroundEffect {
    pub fn new() -> Self {
        Self {
            nonxray: FramebufferEffect::new(),
            damage: ExtraDamage::new(),
            last_visual: None,
            last_capture: None,
        }
    }

    /// Damage the background effect, for example when a blur subregion changes.
    pub fn damage(&mut self) {
        self.damage.damage_all();
        self.nonxray.damage();
    }

    /// Track damage when the resolved plan fingerprints change. Does not store
    /// a parallel mutable options owner — the immutable plan is the only source
    /// of visual parameters at render time (R12).
    ///
    /// The two fingerprints drive two invalidation channels (P03):
    /// - any visual change damages the ExtraDamage element so the effect is repainted with fresh
    ///   uniforms;
    /// - only capture-key changes (blur kernel/on-off/xray) additionally bump the live effect
    ///   commit, which is what forces the damage tracker to re-run `capture_framebuffer` (blit +
    ///   blur pyramid). Draw-only material animation (interaction / material_alpha easing)
    ///   therefore reuses the cached blurred texture.
    pub fn note_plan_keys(
        &mut self,
        visual: ResolvedEffectVisualKey,
        capture: ResolvedEffectCaptureKey,
    ) {
        if self.last_visual.as_ref() != Some(&visual) {
            self.last_visual = Some(visual);
            self.damage.damage_all();
        }
        if self.last_capture.as_ref() != Some(&capture) {
            self.last_capture = Some(capture);
            self.nonxray.damage();
        }
    }

    /// T-32 R-3b: whether the band moved more than the fast-motion threshold
    /// since the last noted capture. Used to promote the downsample tier while
    /// the band crosses cells quickly; the tier is temporary and flips back
    /// through the capture-key invalidation contract (P06). Displacement is
    /// measured on the *quantized* band locations (multiples of 8px), so the
    /// 24px threshold corresponds to three 8px cells.
    pub fn is_fast_motion(&self, capture_band: Rectangle<i32, smithay::utils::Physical>) -> bool {
        self.last_capture.as_ref().is_some_and(|last| {
            let dx = (last.capture_geometry.loc.x - capture_band.loc.x).abs();
            let dy = (last.capture_geometry.loc.y - capture_band.loc.y).abs();
            dx + dy > blur::FAST_MOTION_DISPLACEMENT_PX
        })
    }

    /// Render using an immutable plan only. No config fallback or radius rewrite.
    pub fn render(
        &self,
        ctx: RenderCtx<GlesRenderer>,
        ns: Option<usize>,
        surface_id: Option<u32>,
        plan: &ResolvedEffectPlan,
        xray_pos: XrayPos,
        push: &mut dyn FnMut(BackgroundEffectElement),
    ) {
        if !plan.is_visible() {
            return;
        }

        let damage = self.damage.render(plan.params.geometry);

        if plan.xray {
            let Some(xray) = ctx.xray else {
                return;
            };

            push(damage.into());
            xray.render(
                ctx,
                plan.params.clone(),
                xray_pos,
                plan.blur,
                plan.noise,
                plan.saturation,
                plan.glass,
                BlurTrace {
                    surface_id,
                    namespace: ns,
                },
                &mut |elem| push(elem.into()),
            );
        } else {
            self.render_live(ns, surface_id, plan, damage, push);
        }
    }

    /// Live (non-xray) path. The ExtraDamage element goes in first so it sits
    /// *above* the framebuffer effect: its damage repaints the effect but is
    /// excluded from the damage tracker's below-the-effect overlap test, so it
    /// never forces a re-capture on its own (P03).
    fn render_live(
        &self,
        ns: Option<usize>,
        surface_id: Option<u32>,
        plan: &ResolvedEffectPlan,
        damage: ExtraDamage,
        push: &mut dyn FnMut(BackgroundEffectElement),
    ) {
        push(damage.into());
        let elem = self.nonxray.render(
            ns,
            plan.params.clone(),
            plan.blur_options,
            plan.noise,
            plan.saturation,
            plan.glass,
            BlurTrace {
                surface_id,
                namespace: ns,
            },
        );
        push(elem.into());
    }
}

fn render_params_for_tile(
    geometry: Rectangle<f64, Logical>,
    scale: f64,
    clip_to_geometry: bool,
    alpha: f32,
    block_out: bool,
    blur_region: Option<Arc<Vec<Rectangle<i32, Logical>>>>,
    client_blur_region_geometry: ClientBlurRegionGeometry,
    surface_geo: Rectangle<f64, Logical>,
    surface_anim_scale: Scale<f64>,
) -> Option<RenderParams> {
    // Effects not requested by the surface itself are drawn to match the geometry.
    let mut clip = true;

    let mut effect_geometry = geometry;
    let mut clip_geometry = geometry;
    let mut subregion = None;
    if let Some(rects) = blur_region {
        if rects.is_empty() {
            // Surface has a set, but empty blur region.
            return None;
        } else {
            // If the surface itself requests the effects, apply different defaults.
            clip = clip_to_geometry;

            // Use geometry-shaped blur for blocked-out windows to avoid unintentionally
            // leaking any surface shapes. We render those windows as geometry-shaped solid
            // rectangles anyway.
            if block_out {
                clip = true;
            } else {
                let mut surface_geo = surface_geo.upscale(surface_anim_scale);
                surface_geo.loc += geometry.loc;

                if client_blur_region_geometry == ClientBlurRegionGeometry::BoundingBox {
                    let bbox = transformed_blur_region_bounding_box(
                        &rects,
                        surface_geo,
                        surface_anim_scale,
                        scale,
                    )?;
                    effect_geometry = bbox;
                    clip_geometry = bbox;
                    clip = true;
                } else {
                    let surface_geo = surface_geo
                        .to_physical_precise_round(scale)
                        .to_logical(scale);
                    effect_geometry = surface_geo;
                }

                subregion = Some(TransformedRegion {
                    rects,
                    scale: surface_anim_scale,
                    offset: surface_geo.loc,
                });
            }
        }
    }

    // Clip radius is filled by ResolvedEffectPlan::build (R12), not at render time.
    let clip = clip.then_some((clip_geometry, CornerRadius::default()));

    Some(RenderParams {
        geometry: effect_geometry,
        alpha,
        subregion,
        clip,
        scale,
        draw_clip: None,
        capture_band: None,
    })
}

fn transformed_blur_region_bounding_box(
    rects: &[Rectangle<i32, Logical>],
    surface_geo: Rectangle<f64, Logical>,
    surface_anim_scale: Scale<f64>,
    scale: f64,
) -> Option<Rectangle<f64, Logical>> {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    let mut any = false;

    for rect in rects {
        if rect.is_empty() {
            continue;
        }

        let Some(x2) = rect.loc.x.checked_add(rect.size.w) else {
            continue;
        };
        let Some(y2) = rect.loc.y.checked_add(rect.size.h) else {
            continue;
        };

        if x2 <= rect.loc.x || y2 <= rect.loc.y {
            continue;
        }

        let mut a = rect.loc.to_f64();
        let mut b = Point::new(x2, y2).to_f64();

        a = a.upscale(surface_anim_scale);
        b = b.upscale(surface_anim_scale);

        a += surface_geo.loc;
        b += surface_geo.loc;

        let rect = Rectangle::from_extremities(a, b);
        let Some(rect) = rect.intersection(surface_geo) else {
            continue;
        };

        if rect.is_empty() {
            continue;
        }

        let x2 = rect.loc.x + rect.size.w;
        let y2 = rect.loc.y + rect.size.h;

        min_x = min_x.min(rect.loc.x);
        min_y = min_y.min(rect.loc.y);
        max_x = max_x.max(x2);
        max_y = max_y.max(y2);
        any = true;
    }

    if !any {
        return None;
    }

    let bbox = Rectangle::from_extremities(Point::new(min_x, min_y), Point::new(max_x, max_y));
    Some(bbox.to_physical_precise_round(scale).to_logical(scale))
}

#[cfg(test)]
fn blur_region_bounding_box(rects: &[Rectangle<i32, Logical>]) -> Option<Rectangle<i32, Logical>> {
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    let mut any = false;

    for rect in rects {
        if rect.is_empty() {
            continue;
        }

        let Some(x2) = rect.loc.x.checked_add(rect.size.w) else {
            continue;
        };
        let Some(y2) = rect.loc.y.checked_add(rect.size.h) else {
            continue;
        };

        if x2 <= rect.loc.x || y2 <= rect.loc.y {
            continue;
        }

        min_x = min_x.min(rect.loc.x);
        min_y = min_y.min(rect.loc.y);
        max_x = max_x.max(x2);
        max_y = max_y.max(y2);
        any = true;
    }

    if !any {
        return None;
    }

    Some(Rectangle::from_extremities(
        Point::new(min_x, min_y),
        Point::new(max_x, max_y),
    ))
}

/// Per-surface background effect stored in its data map.
struct SurfaceBackgroundEffect(Mutex<BackgroundEffect>);

impl SurfaceBackgroundEffect {
    fn get(states: &SurfaceData) -> &Self {
        states
            .data_map
            .get_or_insert(|| SurfaceBackgroundEffect(Mutex::new(BackgroundEffect::new())))
    }
}

pub fn damage_surface(states: &SurfaceData) {
    if let Some(effect) = states.data_map.get::<SurfaceBackgroundEffect>() {
        effect.0.lock().unwrap().damage();
    }
}

// Silence, Clippy
// A Smithay user is talking
#[allow(clippy::too_many_arguments)]
pub fn render_for_tile(
    ctx: RenderCtx<GlesRenderer>,
    ns: Option<usize>,
    geometry: Rectangle<f64, Logical>,
    scale: f64,
    clip_to_geometry: bool,
    alpha: f32,
    surface: &WlSurface,
    surface_off: Point<f64, Logical>,
    surface_anim_scale: Scale<f64>,
    client_blur_region_geometry: ClientBlurRegionGeometry,
    blur_config: niri_config::Blur,
    radius: CornerRadius,
    effect: niri_config::BackgroundEffect,
    should_block_out: bool,
    xray_pos: XrayPos,
    geometry_animating: bool,
    push: &mut dyn FnMut(BackgroundEffectElement),
) {
    with_states(surface, |states| {
        let background_effect = SurfaceBackgroundEffect::get(states);
        let mut background_effect = background_effect.0.lock().unwrap();

        let blur_region = get_cached_blur_region(states);
        let has_blur_region = blur_region.as_ref().is_some_and(|r| !r.is_empty());

        let mut surface_geo = surface_geo(states).unwrap_or_default().to_f64();
        surface_geo.loc += surface_off;

        let Some(mut params) = render_params_for_tile(
            geometry,
            scale,
            clip_to_geometry,
            alpha,
            should_block_out,
            blur_region,
            client_blur_region_geometry,
            surface_geo,
            surface_anim_scale,
        ) else {
            return;
        };

        // R12: single resolve before GPU path — no update_config / radius rewrite order.
        let visual = ResolvedEffectPlan::visual_key(blur_config, effect, has_blur_region, radius);
        let precise_band = params.geometry.to_physical_precise_round(scale);
        // T-32 R-3a: while geometry animates, capture on an 8px-grid superset so sub-cell
        // band motion reuses the cached blur (strict superset — draw maps the precise band).
        let capture_band =
            ResolvedEffectPlan::resolve_capture_band(precise_band, geometry_animating);
        if capture_band != precise_band {
            params.capture_band = Some(capture_band);
        }
        let fast_motion = background_effect.is_fast_motion(capture_band);
        let capture = ResolvedEffectPlan::capture_key(
            blur_config,
            effect,
            has_blur_region,
            geometry_animating,
            fast_motion,
            capture_band,
        );
        background_effect.note_plan_keys(visual, capture);

        let Some(plan) = ResolvedEffectPlan::build(
            blur_config,
            effect,
            has_blur_region,
            radius,
            params,
            geometry_animating,
            fast_motion,
        ) else {
            return;
        };

        let xray_pos = xray_pos.offset(plan.params.geometry.loc - geometry.loc);
        background_effect.render(
            ctx,
            ns,
            Some(surface.id().protocol_id()),
            &plan,
            xray_pos,
            push,
        );
    });
}

#[cfg(test)]
mod tests {
    use smithay::backend::renderer::element::Element as _;
    use smithay::utils::{Logical, Point, Rectangle, Scale, Size};

    use super::{blur_region_bounding_box, transformed_blur_region_bounding_box, *};
    use crate::render_helpers::resolved_effect_plan::{quantize_capture_band, ResolvedEffectPlan};

    /// A live (non-xray) tahoe-like effect config: blur on, xray explicitly off.
    fn live_effect() -> niri_config::BackgroundEffect {
        niri_config::BackgroundEffect {
            blur: Some(true),
            xray: Some(false),
            tint_amount: Some(0.3),
            refraction: Some(0.2),
            ..Default::default()
        }
    }

    fn live_params() -> RenderParams {
        RenderParams {
            geometry: Rectangle::new(Point::from((10., 20.)), Size::from((200., 100.))),
            alpha: 1.,
            subregion: None,
            clip: Some((
                Rectangle::new(Point::from((14., 24.)), Size::from((192., 92.))),
                CornerRadius::default(),
            )),
            scale: 1.,
            draw_clip: None,
            capture_band: None,
        }
    }

    fn test_capture_band() -> Rectangle<i32, smithay::utils::Physical> {
        Rectangle::new(Point::from((0, 0)), Size::from((200, 100)))
    }

    fn commits(effect: &BackgroundEffect) -> (CommitCounterProbe, CommitCounterProbe) {
        let fb = effect
            .nonxray
            .render(
                None,
                live_params(),
                None,
                0.,
                1.,
                GlassOptions::default(),
                BlurTrace::default(),
            )
            .current_commit();
        let damage = effect
            .damage
            .render(live_params().geometry)
            .current_commit();
        (CommitCounterProbe(fb), CommitCounterProbe(damage))
    }

    struct CommitCounterProbe(smithay::backend::renderer::utils::CommitCounter);

    impl CommitCounterProbe {
        fn advanced_by(&self, later: &Self) -> Option<usize> {
            later.0.distance(Some(self.0))
        }
    }

    /// P03: draw-only material changes (interaction / material_alpha easing)
    /// must repaint via ExtraDamage but must not bump the live effect commit,
    /// so the damage tracker reuses the cached blurred texture.
    #[test]
    fn draw_only_material_change_damages_without_recapture() {
        let mut effect = BackgroundEffect::new();
        let blur = niri_config::Blur::default();

        let base = live_effect();
        effect.note_plan_keys(
            ResolvedEffectPlan::visual_key(blur, base, true, CornerRadius::default()),
            ResolvedEffectPlan::capture_key(blur, base, true, false, false, test_capture_band()),
        );
        let (fb0, dmg0) = commits(&effect);

        // Simulate one frame of compositor-side material easing: interaction
        // boosts the refractive scalars, material_alpha fades tint.
        let mut eased = base;
        eased.tint_amount = Some(0.15);
        eased.refraction = Some(0.31);
        eased.contrast = Some(1.05);
        effect.note_plan_keys(
            ResolvedEffectPlan::visual_key(blur, eased, true, CornerRadius::default()),
            ResolvedEffectPlan::capture_key(blur, eased, true, false, false, test_capture_band()),
        );
        let (fb1, dmg1) = commits(&effect);

        assert_eq!(
            fb0.advanced_by(&fb1),
            Some(0),
            "draw-only change must not force a framebuffer re-capture"
        );
        assert_eq!(
            dmg0.advanced_by(&dmg1),
            Some(1),
            "draw-only change must still repaint via ExtraDamage"
        );
    }

    /// P03: blur-kernel changes alter the pyramid output, so they must bump
    /// the live effect commit (re-blit + re-blur) as well as repaint.
    #[test]
    fn blur_kernel_change_forces_recapture() {
        let mut effect = BackgroundEffect::new();
        let base = live_effect();
        let blur = niri_config::Blur::default();

        effect.note_plan_keys(
            ResolvedEffectPlan::visual_key(blur, base, true, CornerRadius::default()),
            ResolvedEffectPlan::capture_key(blur, base, true, false, false, test_capture_band()),
        );
        let (fb0, dmg0) = commits(&effect);

        let stronger = niri_config::Blur {
            passes: blur.passes + 1,
            ..blur
        };
        effect.note_plan_keys(
            ResolvedEffectPlan::visual_key(stronger, base, true, CornerRadius::default()),
            ResolvedEffectPlan::capture_key(
                stronger,
                base,
                true,
                false,
                false,
                test_capture_band(),
            ),
        );
        let (fb1, dmg1) = commits(&effect);

        assert_eq!(
            fb0.advanced_by(&fb1),
            Some(1),
            "kernel change must force a re-capture"
        );
        assert_eq!(dmg0.advanced_by(&dmg1), Some(1));
    }

    /// External damage (blur subregion changes) stays conservative on both
    /// channels.
    #[test]
    fn explicit_damage_hits_both_channels() {
        let mut effect = BackgroundEffect::new();
        let (fb0, dmg0) = commits(&effect);
        effect.damage();
        let (fb1, dmg1) = commits(&effect);
        assert_eq!(fb0.advanced_by(&fb1), Some(1));
        assert_eq!(dmg0.advanced_by(&dmg1), Some(1));
    }

    /// A moved capture band invalidates the cached blurred texture: drawing a
    /// texture blitted from one band onto a different destination stretches
    /// stale content (the "milky popup" / darkening edge-reveal artifacts).
    /// Any band change must bump the live effect commit and re-capture; an
    /// unchanged band must keep the cache.
    #[test]
    fn capture_band_change_forces_recapture() {
        let mut effect = BackgroundEffect::new();
        let blur = niri_config::Blur::default();
        let base = live_effect();
        let radius = CornerRadius::default();

        let band_a = Rectangle::new(Point::from((100, 40)), Size::from((360, 480)));
        effect.note_plan_keys(
            ResolvedEffectPlan::visual_key(blur, base, true, radius),
            ResolvedEffectPlan::capture_key(blur, base, true, false, false, band_a),
        );
        let (fb0, dmg0) = commits(&effect);

        // Same band: cached blurred texture stays valid.
        effect.note_plan_keys(
            ResolvedEffectPlan::visual_key(blur, base, true, radius),
            ResolvedEffectPlan::capture_key(blur, base, true, false, false, band_a),
        );
        let (fb1, _) = commits(&effect);
        assert_eq!(
            fb0.advanced_by(&fb1),
            Some(0),
            "an unchanged capture band must reuse the cached texture"
        );

        // Band moved by one physical pixel (settling spring crossing a pixel,
        // panel resize, moving slide frame): must re-capture.
        let band_b = Rectangle::new(Point::from((100, 39)), Size::from((360, 480)));
        effect.note_plan_keys(
            ResolvedEffectPlan::visual_key(blur, base, true, radius),
            ResolvedEffectPlan::capture_key(blur, base, true, false, false, band_b),
        );
        let (fb2, dmg2) = commits(&effect);
        assert_eq!(
            fb1.advanced_by(&fb2),
            Some(1),
            "a moved capture band must force a re-capture"
        );
        // Repaint flows from the effect's own commit bump; the visual
        // fingerprint (material/kernel/radius) did not change.
        assert_eq!(dmg0.advanced_by(&dmg2), Some(0));
    }

    /// T-32 R-3a: while geometry animates, the capture band is the 8px-grid
    /// superset — sub-cell band motion produces the same quantized band, so the
    /// capture key (and the live effect commit) stays put; crossing a cell
    /// forces a re-capture. (Runtime blits are additionally driven by the
    /// damage tracker for i32-geometry changes; this test locks the key/commit
    /// channel.)
    #[test]
    fn quantized_band_reuses_capture_within_cell() {
        let mut effect = BackgroundEffect::new();
        let blur = niri_config::Blur::default();
        let base = live_effect();
        let radius = CornerRadius::default();

        let band = Rectangle::new(Point::from((100, 40)), Size::from((360, 480)));
        let key = |b: Rectangle<i32, smithay::utils::Physical>| {
            ResolvedEffectPlan::capture_key(blur, base, true, true, false, b)
        };

        // Sub-cell move: same quantized band → same key → cache reuse.
        let q1 = quantize_capture_band(band);
        effect.note_plan_keys(
            ResolvedEffectPlan::visual_key(blur, base, true, radius),
            key(q1),
        );
        let (fb0, _) = commits(&effect);
        let moved = Rectangle::new(Point::from((100 + 3, 40)), Size::from((360, 480)));
        let q2 = quantize_capture_band(moved);
        assert_eq!(q1, q2, "3px move stays inside the same 8px cells");
        effect.note_plan_keys(
            ResolvedEffectPlan::visual_key(blur, base, true, radius),
            key(q2),
        );
        let (fb1, _) = commits(&effect);
        assert_eq!(
            fb0.advanced_by(&fb1),
            Some(0),
            "same quantized band must reuse the cached texture"
        );

        // Cell crossing → key changes → re-capture.
        let crossed = Rectangle::new(Point::from((100 + 8, 40)), Size::from((360, 480)));
        let q3 = quantize_capture_band(crossed);
        assert_ne!(q1, q3, "8px move crosses into the next cell");
        effect.note_plan_keys(
            ResolvedEffectPlan::visual_key(blur, base, true, radius),
            key(q3),
        );
        let (fb2, _) = commits(&effect);
        assert_eq!(
            fb1.advanced_by(&fb2),
            Some(1),
            "a quantized band cell crossing must force a re-capture"
        );
    }

    /// T-32 R-3b: the fast-motion tier flip engages and disengages through the
    /// capture key (P06 contract), forcing a re-capture in both directions.
    #[test]
    fn fast_motion_tier_flip_forces_recapture_both_ways() {
        let mut effect = BackgroundEffect::new();
        let blur = niri_config::Blur {
            passes: 3,
            ..niri_config::Blur::default()
        };
        let base = live_effect();
        let radius = CornerRadius::default();
        let band = Rectangle::new(Point::from((100, 40)), Size::from((360, 480)));

        let static_key = ResolvedEffectPlan::capture_key(blur, base, true, false, false, band);
        let fast_key = ResolvedEffectPlan::capture_key(blur, base, true, false, true, band);

        effect.note_plan_keys(
            ResolvedEffectPlan::visual_key(blur, base, true, radius),
            static_key,
        );
        let (fb0, _) = commits(&effect);
        effect.note_plan_keys(
            ResolvedEffectPlan::visual_key(blur, base, true, radius),
            fast_key,
        );
        let (fb1, _) = commits(&effect);
        assert_eq!(fb0.advanced_by(&fb1), Some(1), "engage must re-capture");
        effect.note_plan_keys(
            ResolvedEffectPlan::visual_key(blur, base, true, radius),
            static_key,
        );
        let (fb2, _) = commits(&effect);
        assert_eq!(fb1.advanced_by(&fb2), Some(1), "disengage must re-capture");
    }

    /// T-32 R-3b: fast-motion detection compares against the last noted band.
    #[test]
    fn is_fast_motion_threshold() {
        let mut effect = BackgroundEffect::new();
        let blur = niri_config::Blur::default();
        let base = live_effect();
        let radius = CornerRadius::default();
        let band = Rectangle::new(Point::from((100, 40)), Size::from((360, 480)));

        effect.note_plan_keys(
            ResolvedEffectPlan::visual_key(blur, base, true, radius),
            ResolvedEffectPlan::capture_key(blur, base, true, false, false, band),
        );

        let near = Rectangle::new(Point::from((100 + 20, 40)), Size::from((360, 480)));
        assert!(!effect.is_fast_motion(near), "20px is below the threshold");
        let far = Rectangle::new(Point::from((100 + 25, 40)), Size::from((360, 480)));
        assert!(effect.is_fast_motion(far), "25px exceeds the threshold");
        // No previous note yet → never fast.
        assert!(!BackgroundEffect::new().is_fast_motion(band));
    }

    /// P03: on the live path the ExtraDamage element must be pushed before
    /// (above) the framebuffer effect so its damage never enters the damage
    /// tracker's below-the-effect overlap test.
    #[test]
    fn live_path_pushes_extra_damage_above_framebuffer_effect() {
        let effect = BackgroundEffect::new();
        let blur = niri_config::Blur::default();
        let plan = ResolvedEffectPlan::build(
            blur,
            live_effect(),
            true,
            CornerRadius::default(),
            live_params(),
            false,
            false,
        )
        .expect("live plan must be visible");
        assert!(!plan.xray, "test config must resolve to the live path");

        let damage = effect.damage.render(plan.params.geometry);
        let mut elements = Vec::new();
        effect.render_live(None, None, &plan, damage, &mut |elem| elements.push(elem));

        assert_eq!(elements.len(), 2);
        assert!(
            matches!(elements[0], BackgroundEffectElement::ExtraDamage(_)),
            "ExtraDamage must be above the effect"
        );
        assert!(
            matches!(elements[1], BackgroundEffectElement::FramebufferEffect(_)),
            "framebuffer effect must be below the ExtraDamage element"
        );
    }

    /// P06: a geometry-animation tier flip changes the capture key, so both
    /// the engage and the disengage frame bump the live effect commit even
    /// when nothing else damages the region. Without this, spring sub-pixel
    /// tails or alpha-only animation endings would strand a half-resolution
    /// capture on an otherwise static panel.
    #[test]
    fn downsample_tier_flip_forces_recapture_both_ways() {
        let mut effect = BackgroundEffect::new();
        let base = live_effect();
        let blur = niri_config::Blur {
            passes: 3,
            ..niri_config::Blur::default()
        };
        let radius = CornerRadius::default();

        effect.note_plan_keys(
            ResolvedEffectPlan::visual_key(blur, base, true, radius),
            ResolvedEffectPlan::capture_key(blur, base, true, false, false, test_capture_band()),
        );
        let (fb0, dmg0) = commits(&effect);

        // Engage frame: the open animation starts moving the surface.
        effect.note_plan_keys(
            ResolvedEffectPlan::visual_key(blur, base, true, radius),
            ResolvedEffectPlan::capture_key(blur, base, true, true, false, test_capture_band()),
        );
        let (fb1, dmg1) = commits(&effect);
        assert_eq!(
            fb0.advanced_by(&fb1),
            Some(1),
            "engage frame must force a re-capture at the animation tier"
        );

        // Disengage frame: rest state after the animation completes.
        effect.note_plan_keys(
            ResolvedEffectPlan::visual_key(blur, base, true, radius),
            ResolvedEffectPlan::capture_key(blur, base, true, false, false, test_capture_band()),
        );
        let (fb2, dmg2) = commits(&effect);
        assert_eq!(
            fb1.advanced_by(&fb2),
            Some(1),
            "disengage frame must force a re-capture at the full tier"
        );

        // The visual fingerprint is untouched by the tier: repaint flows from
        // the effect element's own commit bump, not from ExtraDamage.
        assert_eq!(dmg0.advanced_by(&dmg1), Some(0));
        assert_eq!(dmg1.advanced_by(&dmg2), Some(0));
    }

    /// P06: single-pass kernels cannot trade a pass for resolution, so a
    /// geometry animation must leave their capture key untouched (no tier, no
    /// spurious re-captures).
    #[test]
    fn single_pass_kernel_never_engages_downsample_tier() {
        let base = live_effect();
        let blur = niri_config::Blur {
            passes: 1,
            ..niri_config::Blur::default()
        };
        assert_eq!(
            ResolvedEffectPlan::capture_key(blur, base, true, false, false, test_capture_band()),
            ResolvedEffectPlan::capture_key(blur, base, true, true, false, test_capture_band()),
        );
    }

    #[test]
    fn blur_region_bbox_ignores_empty_and_overflowing_rects() {
        let rects: Vec<Rectangle<i32, Logical>> = vec![
            Rectangle::new(Point::new(10, 20), Size::new(30, 40)),
            Rectangle::new(Point::new(0, 0), Size::new(0, 12)),
            Rectangle::new(Point::new(i32::MAX - 1, 0), Size::new(8, 8)),
            Rectangle::new(Point::new(5, 8), Size::new(5, 2)),
        ];

        let bbox = blur_region_bounding_box(&rects).unwrap();
        assert_eq!(bbox.loc, Point::new(5, 8));
        assert_eq!(bbox.size, Size::new(35, 52));
    }

    #[test]
    fn blur_region_bbox_returns_none_for_no_valid_area() {
        let rects: Vec<Rectangle<i32, Logical>> = vec![
            Rectangle::new(Point::new(0, 0), Size::new(0, 10)),
            Rectangle::new(Point::new(i32::MAX - 1, 0), Size::new(8, 8)),
        ];

        assert!(blur_region_bounding_box(&rects).is_none());
    }

    #[test]
    fn transformed_blur_region_bbox_ignores_rects_outside_surface() {
        let rects: Vec<Rectangle<i32, Logical>> = vec![
            Rectangle::new(Point::new(50, 40), Size::new(20, 10)),
            Rectangle::new(Point::new(10_000, 40), Size::new(20, 10)),
        ];
        let surface_geo = Rectangle::new(Point::new(0., 0.), Size::new(200., 100.));

        let bbox =
            transformed_blur_region_bounding_box(&rects, surface_geo, Scale::from(1.), 1.).unwrap();

        assert_eq!(bbox.loc, Point::new(50., 40.));
        assert_eq!(bbox.size, Size::new(20., 10.));
    }

    #[test]
    fn transformed_blur_region_bbox_clamps_partially_outside_rects() {
        let rects: Vec<Rectangle<i32, Logical>> = vec![
            Rectangle::new(Point::new(180, 80), Size::new(50, 40)),
            Rectangle::new(Point::new(i32::MAX - 1, 0), Size::new(8, 8)),
        ];
        let surface_geo = Rectangle::new(Point::new(10., 20.), Size::new(200., 100.));

        let bbox =
            transformed_blur_region_bounding_box(&rects, surface_geo, Scale::from(1.), 1.).unwrap();

        assert_eq!(bbox.loc, Point::new(190., 100.));
        assert_eq!(bbox.size, Size::new(20., 20.));
    }
}
