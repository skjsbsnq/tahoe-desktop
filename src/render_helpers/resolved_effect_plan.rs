//! Immutable resolved background-effect plan (R12).
//!
//! Built once before entering GPU render primitives. Contains fully resolved
//! material parameters, blur kernel choice, noise/saturation fallbacks, and
//! clip radii — so `BackgroundEffect::render` no longer re-selects defaults
//! or depends on `update_config` / corner-radius setter ordering.

use niri_config::CornerRadius;
use smithay::utils::{Logical, Rectangle};

use crate::render_helpers::background_effect::{GlassOptions, Options, RenderParams};
use crate::render_helpers::blur::BlurOptions;

/// Fully resolved visual + geometry inputs for one background-effect draw.
///
/// All Option-style material defaults and blur-config fallbacks are applied
/// during construction. The renderer must not re-decide noise, saturation,
/// blur on/off, xray default, or clip radius from live config objects.
#[derive(Debug, Clone)]
pub struct ResolvedEffectPlan {
    /// Visible/sample geometry, alpha, subregion, clip, draw_clip, scale.
    /// Clip radius (if present) is already expanded and fit.
    pub params: RenderParams,
    /// Whether blur passes run (blur flag and global blur off already applied).
    pub blur: bool,
    /// Kernel when `blur` is true; `None` when blur is off.
    pub blur_options: Option<BlurOptions>,
    pub noise: f32,
    pub saturation: f32,
    pub glass: GlassOptions,
    pub xray: bool,
}

/// Inputs that fully determine a plan's visual half (for damage fingerprinting).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedEffectVisualKey {
    pub options: Options,
    pub blur_config: niri_config::Blur,
    pub corner_radius: CornerRadius,
}

impl ResolvedEffectPlan {
    pub fn is_visible(&self) -> bool {
        self.xray
            || self.blur
            || self.noise > 0.
            || self.saturation != 1.
            || self.glass.is_visible()
    }

    /// Resolve blur/material options exactly once (same rules as former
    /// `BackgroundEffect::update_render_elements` + render-time noise/sat fallback).
    pub fn resolve_options(
        blur_config: niri_config::Blur,
        effect: niri_config::BackgroundEffect,
        has_blur_region: bool,
    ) -> Options {
        let blur = if has_blur_region {
            effect.blur != Some(false)
        } else {
            effect.blur == Some(true)
        };

        let mut options = Options {
            blur,
            xray: effect.xray == Some(true),
            noise: effect.noise,
            saturation: effect.saturation,
            glass: GlassOptions::from_effect(effect),
        };

        // If we have some background effect but xray wasn't explicitly set, default it to true
        // since it's cheaper. (Preserved from pre-R12 behavior.)
        if options.is_visible() && effect.xray.is_none() {
            options.xray = true;
        }

        // Ignore unused blur_config here; kernel/noise defaults applied in `build`.
        let _ = blur_config;
        options
    }

    /// Build an immutable plan. Returns `None` when nothing is visible.
    pub fn build(
        blur_config: niri_config::Blur,
        effect: niri_config::BackgroundEffect,
        has_blur_region: bool,
        corner_radius: CornerRadius,
        mut params: RenderParams,
    ) -> Option<Self> {
        let options = Self::resolve_options(blur_config, effect, has_blur_region);
        if !options.is_visible() {
            return None;
        }

        // Clip radius is decided here — not re-written from BackgroundEffect state in render().
        if let Some(clip) = &mut params.clip {
            clip.1 = corner_radius;
        }
        params.fit_clip_radius();

        let blur = options.blur && !blur_config.off;
        let blur_options = blur.then_some(BlurOptions::from(blur_config));
        let noise = if blur { blur_config.noise } else { 0. };
        let noise = options.noise.unwrap_or(noise) as f32;
        let saturation = if blur { blur_config.saturation } else { 1. };
        let saturation = options.saturation.unwrap_or(saturation) as f32;

        Some(Self {
            params,
            blur,
            blur_options,
            noise,
            saturation,
            glass: options.glass,
            xray: options.xray,
        })
    }

    pub fn visual_key(
        blur_config: niri_config::Blur,
        effect: niri_config::BackgroundEffect,
        has_blur_region: bool,
        corner_radius: CornerRadius,
    ) -> ResolvedEffectVisualKey {
        ResolvedEffectVisualKey {
            options: Self::resolve_options(blur_config, effect, has_blur_region),
            blur_config,
            corner_radius,
        }
    }
}

/// CPU golden snapshot of plan fields used by tests (no GPU).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedEffectPlanGolden {
    pub blur: bool,
    pub xray: bool,
    pub noise: f32,
    pub saturation: f32,
    pub glass: GlassOptions,
    pub geometry: Rectangle<f64, Logical>,
    pub has_clip: bool,
    pub clip_radius: Option<CornerRadius>,
    pub alpha: f32,
}

impl From<&ResolvedEffectPlan> for ResolvedEffectPlanGolden {
    fn from(plan: &ResolvedEffectPlan) -> Self {
        Self {
            blur: plan.blur,
            xray: plan.xray,
            noise: plan.noise,
            saturation: plan.saturation,
            glass: plan.glass,
            geometry: plan.params.geometry,
            has_clip: plan.params.clip.is_some(),
            clip_radius: plan.params.clip.as_ref().map(|(_, r)| *r),
            alpha: plan.params.alpha,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use niri_config::{BackgroundEffect, Blur, Color};
    use smithay::utils::{Point, Size};

    fn base_params(geo: Rectangle<f64, Logical>, clip: bool) -> RenderParams {
        RenderParams {
            geometry: geo,
            alpha: 1.0,
            subregion: None,
            clip: clip.then_some((geo, CornerRadius::default())),
            scale: 1.0,
            draw_clip: None,
        }
    }

    #[test]
    fn plan_resolves_noise_saturation_from_blur_when_blurred() {
        let blur = Blur {
            off: false,
            passes: 2,
            offset: 1.0,
            noise: 0.25,
            saturation: 0.8,
            ..Blur::default()
        };
        let mut effect = BackgroundEffect::default();
        effect.blur = Some(true);

        let plan = ResolvedEffectPlan::build(
            blur,
            effect,
            false,
            CornerRadius::default(),
            base_params(
                Rectangle::new(Point::from((0., 0.)), Size::from((100., 100.))),
                true,
            ),
        )
        .expect("visible");

        assert!(plan.blur);
        assert!((plan.noise - 0.25).abs() < f32::EPSILON);
        assert!((plan.saturation - 0.8).abs() < f32::EPSILON);
        assert!(plan.xray, "default xray when effect visible and xray unset");
    }

    #[test]
    fn plan_respects_explicit_noise_over_blur_default() {
        let blur = Blur {
            off: false,
            noise: 0.25,
            saturation: 0.8,
            ..Blur::default()
        };
        let mut effect = BackgroundEffect::default();
        effect.blur = Some(true);
        effect.noise = Some(0.1);
        effect.saturation = Some(1.2);
        effect.xray = Some(false);

        let plan = ResolvedEffectPlan::build(
            blur,
            effect,
            false,
            CornerRadius::default(),
            base_params(
                Rectangle::new(Point::from((0., 0.)), Size::from((10., 10.))),
                false,
            ),
        )
        .unwrap();

        assert!(!plan.xray);
        assert!((plan.noise - 0.1).abs() < f32::EPSILON);
        assert!((plan.saturation - 1.2).abs() < f32::EPSILON);
    }

    #[test]
    fn plan_blur_region_defaults_blur_on_unless_explicit_false() {
        let blur = Blur::default();
        let mut effect = BackgroundEffect::default();
        // no effect.blur set
        effect.tint_amount = Some(0.2);
        effect.tint_color = Some(Color::new_unpremul(1., 1., 1., 1.));

        let with_region = ResolvedEffectPlan::resolve_options(blur, effect, true);
        assert!(with_region.blur);

        let without = ResolvedEffectPlan::resolve_options(blur, effect, false);
        assert!(!without.blur);
    }

    #[test]
    fn plan_applies_corner_radius_into_clip_once() {
        let mut effect = BackgroundEffect::default();
        effect.blur = Some(true);
        let radius = CornerRadius {
            top_left: 12.,
            top_right: 12.,
            bottom_right: 4.,
            bottom_left: 4.,
        };
        let geo = Rectangle::new(Point::from((0., 0.)), Size::from((100., 50.)));
        let plan = ResolvedEffectPlan::build(
            Blur {
                off: false,
                ..Blur::default()
            },
            effect,
            false,
            radius,
            base_params(geo, true),
        )
        .unwrap();

        let (_, r) = plan.params.clip.expect("clip present");
        // expanded_by(1) then fit_to — radius must not stay default zero.
        assert!(r.top_left > 0.);
        assert!(r.bottom_right > 0.);
    }

    #[test]
    fn plan_none_when_invisible() {
        let effect = BackgroundEffect::default();
        let plan = ResolvedEffectPlan::build(
            Blur {
                off: true,
                ..Blur::default()
            },
            effect,
            false,
            CornerRadius::default(),
            base_params(
                Rectangle::new(Point::from((0., 0.)), Size::from((1., 1.))),
                false,
            ),
        );
        assert!(plan.is_none());
    }

    #[test]
    fn material_glass_fields_enter_plan_without_render_fallback() {
        let mut effect = BackgroundEffect::default();
        effect.xray = Some(true);
        effect.refraction = Some(0.4);
        effect.tint_amount = Some(0.15);

        let plan = ResolvedEffectPlan::build(
            Blur {
                off: true,
                ..Blur::default()
            },
            effect,
            false,
            CornerRadius::default(),
            base_params(
                Rectangle::new(Point::from((2., 3.)), Size::from((40., 60.))),
                true,
            ),
        )
        .unwrap();

        let golden = ResolvedEffectPlanGolden::from(&plan);
        assert!(golden.xray);
        assert!(!golden.blur);
        assert!((golden.glass.refraction - 0.4).abs() < f32::EPSILON);
        assert!((golden.glass.tint_amount - 0.15).abs() < f32::EPSILON);
        assert_eq!(golden.geometry.loc, Point::from((2., 3.)));
    }
}
