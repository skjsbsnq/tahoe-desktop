//! Immutable resolved background-effect plan (R12).
//!
//! Built once before entering GPU render primitives. Contains fully resolved
//! material parameters, blur kernel choice, noise/saturation fallbacks, and
//! clip radii — so `BackgroundEffect::render` no longer re-selects defaults
//! or depends on `update_config` / corner-radius setter ordering.

use niri_config::CornerRadius;
use smithay::utils::{Logical, Rectangle};

use crate::render_helpers::background_effect::{GlassOptions, Options, RenderParams};
use crate::render_helpers::blur::{self, BlurOptions};

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

/// Inputs that determine the framebuffer-capture half of a plan (P03).
///
/// Only changes here alter the blit + blur-pyramid output, so only they may
/// bump the live `FramebufferEffect` commit and force a re-capture. Draw-only
/// material changes (glass uniforms, noise, saturation, corner radius) must
/// stay out of this key: they invalidate drawn pixels through the ExtraDamage
/// element above the effect, while the cached blurred texture in the effect
/// cache is reused. (`params.alpha` is not tracked by either key — as before
/// P03 it repaints via the co-fading surface content above the effect or the
/// glass scalars that change with it.)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedEffectCaptureKey {
    /// Whether blur passes run (same resolution as [`ResolvedEffectPlan::blur`]).
    pub blur: bool,
    /// Kernel when `blur` is true; `None` when blur is off.
    pub blur_options: Option<BlurOptions>,
    /// Whether the xray path is taken instead of the live framebuffer path.
    pub xray: bool,
}

/// Resolve the blur on/off flag and kernel exactly once, shared between
/// [`ResolvedEffectPlan::build`] and [`ResolvedEffectPlan::capture_key`].
///
/// `geometry_animating` engages the P06 animation-period downsample tier on
/// the resolved kernel. The tier lives inside [`BlurOptions`], and this helper
/// is the only place that sets it, so `build` and `capture_key` can never
/// disagree: a tier flip changes the capture key, which bumps the live effect
/// commit (see `BackgroundEffect::note_plan_keys`) and guarantees both the
/// engage and the disengage frame re-capture — even when nothing else damages
/// the region (spring sub-pixel tails, alpha-only animation endings).
fn resolve_blur(
    options: &Options,
    blur_config: niri_config::Blur,
    geometry_animating: bool,
) -> (bool, Option<BlurOptions>) {
    let blur = options.blur && !blur_config.off;
    let blur_options = blur.then(|| {
        let mut blur_options = BlurOptions::from(blur_config);
        // The tier only engages when a pass can be traded for it: with
        // passes > shift the deepest pyramid level — which sets the perceived
        // blur radius — stays identical between tiers, so the flip is
        // invisible. Single-pass kernels would double their radius instead,
        // so they stay on the static tier. The xray effect buffer builds its
        // own BlurOptions straight from config and is never affected.
        if geometry_animating
            && blur_config.passes.clamp(1, 31) > blur::ANIM_DOWNSAMPLE_SHIFT
            && !blur::anim_downsample_disabled()
        {
            blur_options.downsample_shift = blur::ANIM_DOWNSAMPLE_SHIFT;
        }
        blur_options
    });
    (blur, blur_options)
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
    ///
    /// `geometry_animating` reports that the surface is mid geometry (move /
    /// scale) animation this frame, so the blit region shifts and the blur
    /// pyramid re-runs every frame anyway. Those frames run the pyramid one
    /// downsample tier lower (P06). Alpha-only and material-only animations
    /// must pass `false`: they leave captured pixels valid, and switching
    /// tiers would needlessly invalidate the capture. Callers must feed the
    /// same value to [`Self::capture_key`] — the tier is part of the capture
    /// fingerprint so engage/disengage frames force a re-capture.
    pub fn build(
        blur_config: niri_config::Blur,
        effect: niri_config::BackgroundEffect,
        has_blur_region: bool,
        corner_radius: CornerRadius,
        mut params: RenderParams,
        geometry_animating: bool,
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

        let (blur, blur_options) = resolve_blur(&options, blur_config, geometry_animating);
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

    /// `geometry_animating` must be the same value passed to [`Self::build`]
    /// for this frame: the P06 downsample tier is part of the resolved kernel,
    /// so a tier flip changes this key and forces a re-capture through the
    /// live effect commit even on otherwise damage-free frames.
    pub fn capture_key(
        blur_config: niri_config::Blur,
        effect: niri_config::BackgroundEffect,
        has_blur_region: bool,
        geometry_animating: bool,
    ) -> ResolvedEffectCaptureKey {
        let options = Self::resolve_options(blur_config, effect, has_blur_region);
        let (blur, blur_options) = resolve_blur(&options, blur_config, geometry_animating);
        ResolvedEffectCaptureKey {
            blur,
            blur_options,
            xray: options.xray,
        }
    }
}

/// CPU golden snapshot of plan fields used by tests (no GPU).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedEffectPlanGolden {
    pub blur: bool,
    pub blur_options: Option<BlurOptions>,
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
            blur_options: plan.blur_options,
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
    use niri_config::{BackgroundEffect, Blur, Color};
    use smithay::utils::{Point, Size};

    use super::*;

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
            false,
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
            false,
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
            false,
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
            false,
        );
        assert!(plan.is_none());
    }

    #[test]
    fn capture_key_is_stable_under_material_fade_and_interaction_boost() {
        // Mirrors tahoe_glass::render_region material easing: material_alpha
        // fades tint/contrast/refraction/…, interaction boosts them. None of
        // that may alter the capture key — only draw uniforms change.
        let blur = Blur::default();
        let mut base = BackgroundEffect::default();
        base.blur = Some(true);
        base.xray = Some(false);
        base.tint_amount = Some(0.4);
        base.contrast = Some(1.2);
        base.edge_highlight = Some(0.3);
        base.refraction = Some(0.25);
        base.inner_shadow = Some(0.1);
        base.chromatic = Some(0.05);
        base.lens_depth = Some(0.15);

        let key_base = ResolvedEffectPlan::capture_key(blur, base, true, false);

        // material_alpha = 0.5 fade.
        let mut faded = base;
        faded.tint_amount = Some(0.2);
        faded.contrast = Some(1.1);
        faded.edge_highlight = Some(0.15);
        faded.refraction = Some(0.125);
        faded.inner_shadow = Some(0.05);
        faded.chromatic = Some(0.025);
        faded.lens_depth = Some(0.075);
        assert_eq!(
            key_base,
            ResolvedEffectPlan::capture_key(blur, faded, true, false)
        );

        // interaction = 0.4 boost on top.
        let mut boosted = base;
        boosted.contrast = Some(1.28);
        boosted.edge_highlight = Some(0.42);
        boosted.refraction = Some(0.35);
        boosted.inner_shadow = Some(0.14);
        boosted.chromatic = Some(0.07);
        boosted.lens_depth = Some(0.21);
        assert_eq!(
            key_base,
            ResolvedEffectPlan::capture_key(blur, boosted, true, false)
        );

        // The visual key must still see every one of those changes.
        let radius = CornerRadius::default();
        let visual_base = ResolvedEffectPlan::visual_key(blur, base, true, radius);
        assert_ne!(
            visual_base,
            ResolvedEffectPlan::visual_key(blur, faded, true, radius)
        );
        assert_ne!(
            visual_base,
            ResolvedEffectPlan::visual_key(blur, boosted, true, radius)
        );
    }

    #[test]
    fn capture_key_tracks_blur_kernel_but_not_draw_side_blur_config() {
        let mut effect = BackgroundEffect::default();
        effect.blur = Some(true);
        effect.xray = Some(false);
        let blur = Blur::default();
        let key = ResolvedEffectPlan::capture_key(blur, effect, true, false);

        // passes / offset / off feed the pyramid — key must change.
        assert_ne!(
            key,
            ResolvedEffectPlan::capture_key(
                Blur {
                    passes: blur.passes + 1,
                    ..blur
                },
                effect,
                true,
                false,
            )
        );
        assert_ne!(
            key,
            ResolvedEffectPlan::capture_key(
                Blur {
                    offset: blur.offset + 1.,
                    ..blur
                },
                effect,
                true,
                false,
            )
        );
        assert_ne!(
            key,
            ResolvedEffectPlan::capture_key(Blur { off: true, ..blur }, effect, true, false)
        );

        // noise / saturation only feed draw uniforms — key must not change.
        assert_eq!(
            key,
            ResolvedEffectPlan::capture_key(
                Blur {
                    noise: blur.noise + 0.1,
                    saturation: blur.saturation + 0.2,
                    ..blur
                },
                effect,
                true,
                false,
            )
        );

        // Blur on/off through the effect flag must change the key.
        let mut no_blur = effect;
        no_blur.blur = Some(false);
        no_blur.tint_amount = Some(0.2);
        assert_ne!(
            key,
            ResolvedEffectPlan::capture_key(blur, no_blur, true, false)
        );
    }

    #[test]
    fn capture_key_tracks_xray_toggle() {
        let mut live = BackgroundEffect::default();
        live.blur = Some(true);
        live.xray = Some(false);
        let mut xray = live;
        xray.xray = Some(true);

        let blur = Blur::default();
        assert_ne!(
            ResolvedEffectPlan::capture_key(blur, live, true, false),
            ResolvedEffectPlan::capture_key(blur, xray, true, false)
        );
    }

    /// P06: the animation downsample tier is part of the capture key, so the
    /// engage and disengage frames are visible to the capture-invalidation
    /// channel; kernels with nothing to trade (blur off) never flip.
    #[test]
    fn capture_key_tracks_downsample_tier() {
        let mut effect = BackgroundEffect::default();
        effect.blur = Some(true);
        effect.xray = Some(false);
        let blur = Blur {
            passes: 3,
            ..Blur::default()
        };

        let static_key = ResolvedEffectPlan::capture_key(blur, effect, true, false);
        let anim_key = ResolvedEffectPlan::capture_key(blur, effect, true, true);
        assert_ne!(static_key, anim_key);
        assert_eq!(
            static_key.blur_options.expect("blur on").downsample_shift,
            0
        );
        assert_eq!(
            anim_key.blur_options.expect("blur on").downsample_shift,
            blur::ANIM_DOWNSAMPLE_SHIFT
        );

        let off = Blur { off: true, ..blur };
        assert_eq!(
            ResolvedEffectPlan::capture_key(off, effect, true, false),
            ResolvedEffectPlan::capture_key(off, effect, true, true),
        );
    }

    #[test]
    fn capture_key_matches_plan_blur_resolution() {
        // The key and the plan must resolve blur identically (shared helper),
        // on both the static and the animation tier.
        let mut effect = BackgroundEffect::default();
        effect.blur = Some(true);
        effect.xray = Some(false);
        // Keep the plan visible even when the global blur kernel is off.
        effect.tint_amount = Some(0.2);
        effect.tint_color = Some(Color::new_unpremul(1., 1., 1., 1.));

        for blur in [
            Blur::default(),
            Blur {
                off: true,
                ..Blur::default()
            },
            Blur {
                passes: 5,
                offset: 7.,
                ..Blur::default()
            },
            Blur {
                passes: 1,
                ..Blur::default()
            },
        ] {
            for geometry_animating in [false, true] {
                let key = ResolvedEffectPlan::capture_key(blur, effect, true, geometry_animating);
                let plan = ResolvedEffectPlan::build(
                    blur,
                    effect,
                    true,
                    CornerRadius::default(),
                    base_params(
                        Rectangle::new(Point::from((0., 0.)), Size::from((100., 100.))),
                        true,
                    ),
                    geometry_animating,
                )
                .expect("blurred effect stays visible");
                assert_eq!(key.blur, plan.blur);
                assert_eq!(key.blur_options, plan.blur_options);
                assert_eq!(key.xray, plan.xray);
            }
        }
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
            false,
        )
        .unwrap();

        let golden = ResolvedEffectPlanGolden::from(&plan);
        assert!(golden.xray);
        assert!(!golden.blur);
        assert!((golden.glass.refraction - 0.4).abs() < f32::EPSILON);
        assert!((golden.glass.tint_amount - 0.15).abs() < f32::EPSILON);
        assert_eq!(golden.geometry.loc, Point::from((2., 3.)));
    }

    /// P06: a geometry animation engages the blur downsample tier — and only
    /// the tier. Every other resolved plan field must stay identical so the
    /// tier can never change materials, clipping, or xray routing.
    #[test]
    fn geometry_animation_engages_downsample_tier_only() {
        let blur = Blur {
            off: false,
            passes: 3,
            offset: 4.0,
            ..Blur::default()
        };
        let mut effect = BackgroundEffect::default();
        effect.blur = Some(true);
        effect.xray = Some(false);
        let geo = Rectangle::new(Point::from((0., 0.)), Size::from((100., 100.)));

        let static_plan = ResolvedEffectPlan::build(
            blur,
            effect,
            true,
            CornerRadius::default(),
            base_params(geo, true),
            false,
        )
        .expect("visible");
        let anim_plan = ResolvedEffectPlan::build(
            blur,
            effect,
            true,
            CornerRadius::default(),
            base_params(geo, true),
            true,
        )
        .expect("visible");

        let static_options = static_plan.blur_options.expect("blur on");
        let anim_options = anim_plan.blur_options.expect("blur on");
        assert_eq!(static_options.downsample_shift, 0);
        assert_eq!(
            anim_options.downsample_shift,
            crate::render_helpers::blur::ANIM_DOWNSAMPLE_SHIFT
        );

        // The kernel itself is untouched: same passes/offset, one tier down.
        assert_eq!(anim_options.passes, static_options.passes);
        assert_eq!(anim_options.offset, static_options.offset);

        // Only blur_options may differ between the two plans.
        let mut static_golden = ResolvedEffectPlanGolden::from(&static_plan);
        let anim_golden = ResolvedEffectPlanGolden::from(&anim_plan);
        static_golden.blur_options = anim_golden.blur_options;
        assert_eq!(static_golden, anim_golden);
    }

    /// P06: without blur there is nothing to downsample — a geometry
    /// animation must not invent blur options or alter the plan.
    #[test]
    fn geometry_animation_without_blur_keeps_plan_untouched() {
        let blur = Blur {
            off: true,
            ..Blur::default()
        };
        let mut effect = BackgroundEffect::default();
        effect.xray = Some(false);
        effect.tint_amount = Some(0.3);
        effect.tint_color = Some(Color::new_unpremul(1., 1., 1., 1.));
        let geo = Rectangle::new(Point::from((0., 0.)), Size::from((80., 40.)));

        let static_plan = ResolvedEffectPlan::build(
            blur,
            effect,
            false,
            CornerRadius::default(),
            base_params(geo, true),
            false,
        )
        .expect("tinted glass stays visible");
        let anim_plan = ResolvedEffectPlan::build(
            blur,
            effect,
            false,
            CornerRadius::default(),
            base_params(geo, true),
            true,
        )
        .expect("tinted glass stays visible");

        assert!(!anim_plan.blur);
        assert!(anim_plan.blur_options.is_none());
        assert_eq!(
            ResolvedEffectPlanGolden::from(&static_plan),
            ResolvedEffectPlanGolden::from(&anim_plan)
        );
    }
}
