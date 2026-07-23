use std::sync::{Arc, Mutex};

use niri_config::CornerRadius;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Point, Rectangle, Scale};
use smithay::wayland::compositor::{with_states, SurfaceData};
use wayland_server::protocol::wl_surface::WlSurface;

use crate::handlers::background_effect::get_cached_blur_region;
use crate::niri_render_elements;
use crate::render_helpers::damage::ExtraDamage;
use crate::render_helpers::framebuffer_effect::{FramebufferEffect, FramebufferEffectElement};
use crate::render_helpers::resolved_effect_plan::{ResolvedEffectPlan, ResolvedEffectVisualKey};
use crate::render_helpers::xray::{XrayElement, XrayPos};
use crate::render_helpers::RenderCtx;
use crate::utils::region::TransformedRegion;
use crate::utils::surface_geo;

#[derive(Debug)]
pub struct BackgroundEffect {
    nonxray: FramebufferEffect,
    /// Damage when resolved plan visuals change.
    damage: ExtraDamage,
    /// Last resolved visual fingerprint (R12). Geometry is not included;
    /// only material/blur/radius changes need ExtraDamage on the effect.
    last_visual: Option<ResolvedEffectVisualKey>,
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
        }
    }

    /// Damage the background effect, for example when a blur subregion changes.
    pub fn damage(&mut self) {
        self.damage.damage_all();
        self.nonxray.damage();
    }

    /// Track damage when the resolved visual fingerprint changes. Does not store
    /// a parallel mutable options owner — the immutable plan is the only source
    /// of visual parameters at render time (R12).
    pub fn note_plan_visual(&mut self, key: ResolvedEffectVisualKey) {
        if self.last_visual.as_ref() == Some(&key) {
            return;
        }
        self.last_visual = Some(key);
        self.damage.damage_all();
        self.nonxray.damage();
    }

    /// Render using an immutable plan only. No config fallback or radius rewrite.
    pub fn render(
        &self,
        ctx: RenderCtx<GlesRenderer>,
        ns: Option<usize>,
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
                &mut |elem| push(elem.into()),
            );
        } else {
            let elem = self.nonxray.render(
                ns,
                plan.params.clone(),
                plan.blur_options,
                plan.noise,
                plan.saturation,
                plan.glass,
            );
            push(elem.into());
        }
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
    push: &mut dyn FnMut(BackgroundEffectElement),
) {
    with_states(surface, |states| {
        let background_effect = SurfaceBackgroundEffect::get(states);
        let mut background_effect = background_effect.0.lock().unwrap();

        let blur_region = get_cached_blur_region(states);
        let has_blur_region = blur_region.as_ref().is_some_and(|r| !r.is_empty());

        let mut surface_geo = surface_geo(states).unwrap_or_default().to_f64();
        surface_geo.loc += surface_off;

        let Some(params) = render_params_for_tile(
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
        background_effect.note_plan_visual(visual);

        let Some(plan) =
            ResolvedEffectPlan::build(blur_config, effect, has_blur_region, radius, params)
        else {
            return;
        };

        let xray_pos = xray_pos.offset(plan.params.geometry.loc - geometry.loc);
        background_effect.render(ctx, ns, &plan, xray_pos, push);
    });
}

#[cfg(test)]
mod tests {
    use super::{blur_region_bounding_box, transformed_blur_region_bounding_box};
    use smithay::utils::{Logical, Point, Rectangle, Scale, Size};

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
