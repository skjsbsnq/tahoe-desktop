use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::utils::{
    Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::backend::renderer::element::Element;
use smithay::utils::{Logical, Physical, Point, Scale, Size};
use smithay::wayland::shell::wlr_layer::Anchor;

use crate::animation::{Animation, Clock};
use crate::render_helpers::solid_color::SolidColorRenderElement;

pub type OpeningLayerWaylandRenderElement<R> =
    RelocateRenderElement<RescaleRenderElement<WaylandSurfaceRenderElement<R>>>;
pub type OpeningLayerSolidColorRenderElement =
    RelocateRenderElement<RescaleRenderElement<SolidColorRenderElement>>;

#[derive(Debug)]
pub struct OpenAnimation {
    anim: Animation,
    config: niri_config::animations::LayerOpenAnim,
}

#[derive(Debug, Clone, Copy)]
pub struct OpenAnimationState {
    pub alpha: f32,
    scale: f64,
    origin: niri_config::animations::LayerAnimationOrigin,
    edge: niri_config::animations::LayerAnimationEdge,
    offset: f64,
}

impl OpenAnimation {
    pub fn new(clock: Clock, config: niri_config::animations::LayerOpenAnim) -> Self {
        Self {
            anim: Animation::new(clock, 0., 1., 0., config.anim),
            config,
        }
    }

    pub fn is_done(&self) -> bool {
        self.anim.is_done()
    }

    pub fn state(&self) -> OpenAnimationState {
        let progress = self.anim.clamped_value().clamp(0., 1.);
        let config = self.config;

        let alpha = config.opacity_from + (1. - config.opacity_from) * progress as f32;
        let scale = match config.style {
            niri_config::animations::LayerOpenAnimationStyle::Popin => {
                config.scale_from + (1. - config.scale_from) * progress
            }
            niri_config::animations::LayerOpenAnimationStyle::Fade
            | niri_config::animations::LayerOpenAnimationStyle::Slide => 1.,
        };
        let offset = match config.style {
            niri_config::animations::LayerOpenAnimationStyle::Slide => {
                config.distance * (1. - progress)
            }
            niri_config::animations::LayerOpenAnimationStyle::Fade
            | niri_config::animations::LayerOpenAnimationStyle::Popin => 0.,
        };

        OpenAnimationState {
            alpha,
            scale,
            origin: config.origin,
            edge: config.edge,
            offset,
        }
    }
}

impl OpenAnimationState {
    pub fn origin(
        self,
        location: Point<f64, Logical>,
        size: Size<f64, Logical>,
        anchor: Anchor,
        output_scale: Scale<f64>,
    ) -> Point<i32, Physical> {
        let center = location + size.to_point().downscale(2.);
        let origin = match self.origin {
            niri_config::animations::LayerAnimationOrigin::Center => center,
            niri_config::animations::LayerAnimationOrigin::Anchor => Point::new(
                anchor_axis_origin(
                    location.x,
                    size.w,
                    anchor.contains(Anchor::LEFT),
                    anchor.contains(Anchor::RIGHT),
                ),
                anchor_axis_origin(
                    location.y,
                    size.h,
                    anchor.contains(Anchor::TOP),
                    anchor.contains(Anchor::BOTTOM),
                ),
            ),
        };

        origin.to_physical_precise_round(output_scale)
    }

    pub fn offset(self) -> Point<f64, Logical> {
        match self.edge {
            niri_config::animations::LayerAnimationEdge::Top => Point::new(0., -self.offset),
            niri_config::animations::LayerAnimationEdge::Right => Point::new(self.offset, 0.),
            niri_config::animations::LayerAnimationEdge::Bottom => Point::new(0., self.offset),
            niri_config::animations::LayerAnimationEdge::Left => Point::new(-self.offset, 0.),
        }
    }

    pub fn wrap<E: Element>(
        self,
        element: E,
        origin: Point<i32, Physical>,
    ) -> RescaleRenderElement<E> {
        RescaleRenderElement::from_element(element, origin, self.scale)
    }

    pub fn should_wrap(self) -> bool {
        (self.scale - 1.).abs() > f64::EPSILON
    }
}

pub fn wrap<E: Element>(
    element: E,
    state: OpenAnimationState,
    origin: Point<i32, Physical>,
    offset: Point<i32, Physical>,
) -> RelocateRenderElement<RescaleRenderElement<E>> {
    let elem = state.wrap(element, origin);
    RelocateRenderElement::from_element(elem, offset, Relocate::Relative)
}

fn anchor_axis_origin(loc: f64, size: f64, anchored_min: bool, anchored_max: bool) -> f64 {
    match (anchored_min, anchored_max) {
        (true, false) => loc,
        (false, true) => loc + size,
        _ => loc + size / 2.,
    }
}
