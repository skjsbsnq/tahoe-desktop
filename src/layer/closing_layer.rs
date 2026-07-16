use std::time::Duration;

use anyhow::Context as _;
use niri_config::BlockOutFrom;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::utils::{
    CropRenderElement, Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::backend::renderer::element::{Element, Kind, RenderElement};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::Texture;
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size, Transform};
use smithay::wayland::shell::wlr_layer::Anchor;

use crate::animation::{Animation, Clock};
use crate::niri_render_elements;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::snapshot::RenderSnapshot;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::{render_to_encompassing_texture, RenderTarget};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CloseAnimationStartState {
    pub start_alpha: f32,
    pub start_scale: f64,
    pub start_offset: Point<f64, Logical>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CloseAnimationRenderState {
    pub alpha: f32,
    pub scale: f64,
    pub offset: Point<f64, Logical>,
    pub origin: niri_config::animations::LayerAnimationOrigin,
    pub style: niri_config::animations::LayerCloseAnimationStyle,
}

impl CloseAnimationRenderState {
    pub fn should_wrap(self) -> bool {
        (self.scale - 1.).abs() > f64::EPSILON
    }

    pub fn edge_reveal_crop_rect(
        self,
        location: Point<f64, Logical>,
        size: Size<f64, Logical>,
        scale: Scale<f64>,
    ) -> Option<Rectangle<i32, Physical>> {
        if self.style != niri_config::animations::LayerCloseAnimationStyle::EdgeReveal {
            return None;
        }

        Some(Rectangle::new(location, size).to_physical_precise_round(scale))
    }
}

impl Default for CloseAnimationStartState {
    fn default() -> Self {
        Self {
            start_alpha: 1.,
            start_scale: 1.,
            start_offset: Point::from((0., 0.)),
        }
    }
}

#[derive(Debug)]
pub struct ClosingLayer {
    buffer: TextureBuffer<GlesTexture>,
    blocked_out: Option<(TextureBuffer<GlesTexture>, Point<f64, Logical>)>,
    block_out_from: Option<BlockOutFrom>,
    geo_size: Size<f64, Logical>,
    pos: Point<f64, Logical>,
    buffer_offset: Point<f64, Logical>,
    transform_anim: Animation,
    opacity_anim: Animation,
    opacity_delay: Duration,
    config: niri_config::animations::LayerCloseAnim,
    start_alpha: f32,
    start_scale: f64,
    start_offset: Point<f64, Logical>,
    anchor: Anchor,
    /// Absolute (output-local) pointer position for `origin "pointer"`.
    pointer_origin: Option<Point<f64, Logical>>,
}

niri_render_elements! {
    ClosingLayerRenderElement => {
        Texture = RelocateRenderElement<RescaleRenderElement<PrimaryGpuTextureRenderElement>>,
        CroppedTexture = CropRenderElement<RelocateRenderElement<
            RescaleRenderElement<PrimaryGpuTextureRenderElement>
        >>,
    }
}

impl ClosingLayer {
    pub fn new<E: RenderElement<GlesRenderer>>(
        renderer: &mut GlesRenderer,
        snapshot: RenderSnapshot<E, E>,
        scale: Scale<f64>,
        mut geo_size: Size<f64, Logical>,
        pos: Point<f64, Logical>,
        mut clock: Clock,
        config: niri_config::animations::LayerCloseAnim,
        start: CloseAnimationStartState,
        anchor: Anchor,
        pointer_origin: Option<Point<f64, Logical>>,
    ) -> anyhow::Result<Self> {
        let _span = tracy_client::span!("ClosingLayer::new");

        let mut render_to_texture = |elements: Vec<E>| -> anyhow::Result<_> {
            let (texture, _sync_point, geo) = render_to_encompassing_texture(
                renderer,
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                &elements,
            )
            .context("error rendering to texture")?;

            let buffer = TextureBuffer::from_texture(
                renderer,
                texture,
                scale,
                Transform::Normal,
                Vec::new(),
            );

            let offset = geo.loc.to_f64().to_logical(scale);
            Ok((buffer, offset))
        };

        let (buffer, buffer_offset) =
            render_to_texture(snapshot.contents).context("error rendering contents")?;
        let blocked_out = if snapshot.block_out_from.is_some() {
            Some(
                render_to_texture(snapshot.blocked_out_contents)
                    .context("error rendering blocked-out contents")?,
            )
        } else {
            None
        };

        if geo_size.w <= 0. || geo_size.h <= 0. {
            geo_size = snapshot.size;
        }

        if geo_size.w <= 0. || geo_size.h <= 0. {
            let tex_size = buffer.texture().size().to_f64();
            geo_size = Size::new(
                (tex_size.w / scale.x).max(1.),
                (tex_size.h / scale.y).max(1.),
            );
        }

        // Snapshot rendering may block; do not charge that time to the close animation.
        clock.clear();
        let transform_anim = Animation::new(clock.clone(), 0., 1., 0., config.transform_anim);
        let opacity_anim = Animation::new(clock, 0., 1., 0., config.opacity_anim);

        Ok(Self {
            buffer,
            blocked_out,
            block_out_from: snapshot.block_out_from,
            geo_size,
            pos,
            buffer_offset,
            transform_anim,
            opacity_anim,
            opacity_delay: Duration::from_millis(u64::from(config.opacity_delay_ms)),
            config,
            start_alpha: start.start_alpha,
            start_scale: start.start_scale,
            start_offset: start.start_offset,
            anchor,
            pointer_origin: match config.origin {
                niri_config::animations::LayerAnimationOrigin::Pointer => pointer_origin,
                _ => None,
            },
        })
    }

    pub fn advance_animations(&mut self) {
        // Force evaluation so time-based animation state reaches the done condition.
        self.transform_anim.value();
        self.opacity_anim.value();
    }

    pub fn are_animations_ongoing(&self) -> bool {
        !(self.transform_anim.is_done() && self.opacity_anim.is_done_with_delay(self.opacity_delay))
    }

    #[cfg(test)]
    pub fn start_state_for_tests(&self) -> CloseAnimationStartState {
        CloseAnimationStartState {
            start_alpha: self.start_alpha,
            start_scale: self.start_scale,
            start_offset: self.start_offset,
        }
    }

    /// Render the closing snapshot for this frame.
    ///
    /// Returns `None` when edge-reveal crop is active and the transformed snapshot
    /// is completely outside the reveal viewport. That means "do not draw", not
    /// "fall through to the uncropped element".
    pub fn render(
        &self,
        view_rect: Rectangle<f64, Logical>,
        scale: Scale<f64>,
        target: RenderTarget,
    ) -> Option<ClosingLayerRenderElement> {
        let (buffer, offset) = if target.should_block_out(self.block_out_from) {
            let (buffer, offset) = self
                .blocked_out
                .as_ref()
                .expect("blocked-out buffer must exist when block-out is configured");
            (buffer, *offset)
        } else {
            (&self.buffer, self.buffer_offset)
        };

        let state = self.render_state();

        let elem = TextureRenderElement::from_texture_buffer(
            buffer.clone(),
            Point::from((0., 0.)),
            state.alpha.clamp(0., 1.),
            None,
            None,
            Kind::Unspecified,
        );
        let elem = PrimaryGpuTextureRenderElement(elem);

        let center = self.geo_size.to_point().downscale(2.);
        let origin = match state.origin {
            niri_config::animations::LayerAnimationOrigin::Center => center,
            niri_config::animations::LayerAnimationOrigin::Anchor => Point::new(
                anchor_axis_origin(
                    self.geo_size.w,
                    self.anchor.contains(Anchor::LEFT),
                    self.anchor.contains(Anchor::RIGHT),
                ),
                anchor_axis_origin(
                    self.geo_size.h,
                    self.anchor.contains(Anchor::TOP),
                    self.anchor.contains(Anchor::BOTTOM),
                ),
            ),
            // ClosingLayer coords are local to the buffer; convert absolute
            // pointer (output space) into buffer-local by subtracting pos.
            niri_config::animations::LayerAnimationOrigin::Pointer => self
                .pointer_origin
                .map(|p| Point::new(p.x - self.pos.x, p.y - self.pos.y))
                .unwrap_or(center),
        };
        let elem = RescaleRenderElement::from_element(
            elem,
            (origin - offset).to_physical_precise_round(scale),
            state.scale.max(0.),
        );

        let mut location = self.pos + offset + state.offset;
        location.x -= view_rect.loc.x;
        let elem = RelocateRenderElement::from_element(
            elem,
            location.to_physical_precise_round(scale),
            Relocate::Relative,
        );

        let mut crop_location = self.pos;
        crop_location.x -= view_rect.loc.x;
        if let Some(crop_rect) = state.edge_reveal_crop_rect(crop_location, self.geo_size, scale) {
            // Edge-reveal: partial intersection → crop; complete miss → drop.
            // Never fall through to the uncropped element (would flash full surface).
            let intersects = elem
                .geometry(scale)
                .intersection(crop_rect)
                .is_some_and(|rect| !rect.is_empty());
            if !intersects {
                return None;
            }
            let elem = CropRenderElement::from_element(elem, scale, crop_rect).unwrap();
            return Some(elem.into());
        }

        // No edge-reveal crop (fade/slide/pop styles): draw the transformed element.
        Some(elem.into())
    }

    pub fn render_state(&self) -> CloseAnimationRenderState {
        let transform_progress = self.transform_anim.clamped_value().clamp(0., 1.);
        let opacity_progress = self
            .opacity_anim
            .clamped_value_with_delay(self.opacity_delay)
            .clamp(0., 1.);
        let config = self.config;
        let target_alpha = config.opacity_to;
        let alpha = self.start_alpha + (target_alpha - self.start_alpha) * opacity_progress as f32;
        let target_scale = match config.style {
            niri_config::animations::LayerCloseAnimationStyle::Popout
            | niri_config::animations::LayerCloseAnimationStyle::PopSlide => config.scale_to,
            niri_config::animations::LayerCloseAnimationStyle::Fade
            | niri_config::animations::LayerCloseAnimationStyle::Slide
            | niri_config::animations::LayerCloseAnimationStyle::EdgeReveal => 1.,
        };
        let scale = self.start_scale + (target_scale - self.start_scale) * transform_progress;
        let target_offset = match config.style {
            // slide and pop-slide use the configured distance. edge-reveal uses
            // the layer surface extent so close fully retracts the surface.
            niri_config::animations::LayerCloseAnimationStyle::Slide
            | niri_config::animations::LayerCloseAnimationStyle::PopSlide => {
                edge_offset(config.edge, config.distance)
            }
            niri_config::animations::LayerCloseAnimationStyle::EdgeReveal => edge_offset(
                config.edge,
                edge_reveal_distance(config.edge, self.geo_size),
            ),
            niri_config::animations::LayerCloseAnimationStyle::Fade
            | niri_config::animations::LayerCloseAnimationStyle::Popout => Point::from((0., 0.)),
        };
        let offset = Point::new(
            self.start_offset.x + (target_offset.x - self.start_offset.x) * transform_progress,
            self.start_offset.y + (target_offset.y - self.start_offset.y) * transform_progress,
        );

        CloseAnimationRenderState {
            alpha,
            scale,
            offset,
            origin: config.origin,
            style: config.style,
        }
    }

    pub fn position(&self) -> Point<f64, Logical> {
        self.pos
    }
}

fn edge_reveal_distance(
    edge: niri_config::animations::LayerAnimationEdge,
    size: Size<f64, Logical>,
) -> f64 {
    match edge {
        niri_config::animations::LayerAnimationEdge::Top
        | niri_config::animations::LayerAnimationEdge::Bottom => size.h,
        niri_config::animations::LayerAnimationEdge::Left
        | niri_config::animations::LayerAnimationEdge::Right => size.w,
    }
}

fn edge_offset(
    edge: niri_config::animations::LayerAnimationEdge,
    distance: f64,
) -> Point<f64, Logical> {
    match edge {
        niri_config::animations::LayerAnimationEdge::Top => Point::new(0., -distance),
        niri_config::animations::LayerAnimationEdge::Right => Point::new(distance, 0.),
        niri_config::animations::LayerAnimationEdge::Bottom => Point::new(0., distance),
        niri_config::animations::LayerAnimationEdge::Left => Point::new(-distance, 0.),
    }
}

fn anchor_axis_origin(size: f64, anchored_min: bool, anchored_max: bool) -> f64 {
    match (anchored_min, anchored_max) {
        (true, false) => 0.,
        (false, true) => size,
        _ => size / 2.,
    }
}

/// Pure geometry policy for closing edge-reveal crop decisions.
///
/// Mirrors [`ClosingLayer::render`]: no crop → draw; partial → crop geo;
/// complete miss → drop (None). Used by unit tests without a GPU texture.
#[cfg(test)]
pub(crate) fn closing_edge_reveal_draw_geo(
    elem_geo: Rectangle<i32, Physical>,
    crop_rect: Option<Rectangle<i32, Physical>>,
) -> Option<Rectangle<i32, Physical>> {
    let Some(crop_rect) = crop_rect else {
        return Some(elem_geo);
    };
    elem_geo.intersection(crop_rect).filter(|r| !r.is_empty())
}

#[cfg(test)]
mod tests {
    use niri_config::animations::{LayerAnimationOrigin, LayerCloseAnimationStyle};
    use smithay::utils::{Point, Rectangle, Scale, Size};

    use super::*;

    fn edge_reveal_state(offset: Point<f64, Logical>) -> CloseAnimationRenderState {
        CloseAnimationRenderState {
            alpha: 1.,
            scale: 1.,
            offset,
            origin: LayerAnimationOrigin::Center,
            style: LayerCloseAnimationStyle::EdgeReveal,
        }
    }

    fn fade_state() -> CloseAnimationRenderState {
        CloseAnimationRenderState {
            alpha: 1.,
            scale: 1.,
            offset: Point::from((0., 0.)),
            origin: LayerAnimationOrigin::Center,
            style: LayerCloseAnimationStyle::Fade,
        }
    }

    #[test]
    fn no_edge_reveal_keeps_uncropped_draw() {
        let geo = Rectangle::new(Point::from((10, 10)), Size::from((100, 50)));
        assert_eq!(
            closing_edge_reveal_draw_geo(geo, None),
            Some(geo),
            "non-edge-reveal styles must keep drawing"
        );
        assert!(fade_state()
            .edge_reveal_crop_rect(
                Point::from((0., 0.)),
                Size::from((100., 50.)),
                Scale::from(1.),
            )
            .is_none());
    }

    #[test]
    fn partial_intersection_returns_cropped_geo() {
        let crop = Rectangle::new(Point::from((0, 0)), Size::from((100, 100)));
        let elem = Rectangle::new(Point::from((50, 50)), Size::from((100, 100)));
        let drawn = closing_edge_reveal_draw_geo(elem, Some(crop)).expect("partial must draw");
        assert_eq!(
            drawn,
            Rectangle::new(Point::from((50, 50)), Size::from((50, 50)))
        );
    }

    #[test]
    fn complete_miss_returns_none() {
        let crop = Rectangle::new(Point::from((0, 0)), Size::from((100, 100)));
        // Fully below the reveal viewport.
        let elem = Rectangle::new(Point::from((0, 200)), Size::from((100, 50)));
        assert_eq!(
            closing_edge_reveal_draw_geo(elem, Some(crop)),
            None,
            "complete miss must not fall through to uncropped draw"
        );
    }

    #[test]
    fn complete_miss_after_edge_offset_returns_none() {
        // Snap at rest size 200x80, crop is rest viewport; after full bottom
        // retract the relocated geo sits entirely below the crop.
        let scale = Scale::from(1.);
        let geo_size = Size::from((200., 80.));
        let pos = Point::from((10., 20.));
        let state = edge_reveal_state(Point::from((0., 80.))); // full height offset
        let crop = state
            .edge_reveal_crop_rect(pos, geo_size, scale)
            .expect("edge-reveal must produce crop");
        let relocated = Rectangle::new(
            Point::from(((pos.x).round() as i32, (pos.y + 80.).round() as i32)),
            Size::from((200, 80)),
        );
        assert_eq!(closing_edge_reveal_draw_geo(relocated, Some(crop)), None);
    }

    #[test]
    fn fractional_scale_partial_intersection() {
        let scale = Scale::from(1.25);
        let geo_size = Size::from((160., 64.));
        let pos = Point::from((8., 12.));
        let state = edge_reveal_state(Point::from((0., 20.)));
        let crop = state
            .edge_reveal_crop_rect(pos, geo_size, scale)
            .expect("crop");
        // Relocated element still overlaps crop after partial offset.
        let relocated_loc = (pos + Point::from((0., 20.))).to_physical_precise_round(scale);
        let relocated_size = geo_size.to_physical_precise_round(scale);
        let elem = Rectangle::new(relocated_loc, relocated_size);
        let drawn = closing_edge_reveal_draw_geo(elem, Some(crop)).expect("partial");
        assert!(!drawn.is_empty());
        assert!(drawn.intersection(crop).is_some());
    }

    #[test]
    fn inherited_non_one_scale_still_drops_complete_miss() {
        // Non-1.0 animation scale (interrupted open→close) does not change
        // the complete-miss drop policy once geometries are computed.
        let crop = Rectangle::new(Point::from((0, 0)), Size::from((200, 100)));
        // After rescale, geometry is outside crop entirely.
        let post_scale_geo = Rectangle::new(Point::from((400, 0)), Size::from((100, 80)));
        assert_eq!(
            closing_edge_reveal_draw_geo(post_scale_geo, Some(crop)),
            None
        );
    }

    #[test]
    fn empty_intersection_is_miss() {
        // Touching only on a zero-area edge counts as miss.
        let crop = Rectangle::new(Point::from((0, 0)), Size::from((100, 100)));
        let elem = Rectangle::new(Point::from((100, 0)), Size::from((50, 50)));
        let inter = elem.intersection(crop);
        assert!(inter.is_none() || inter.is_some_and(|r| r.is_empty()));
        assert_eq!(closing_edge_reveal_draw_geo(elem, Some(crop)), None);
    }
}
