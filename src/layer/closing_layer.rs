use anyhow::Context as _;
use niri_config::BlockOutFrom;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::utils::{
    Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::backend::renderer::element::{Kind, RenderElement};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::Texture;
use smithay::utils::{Logical, Point, Rectangle, Scale, Size, Transform};
use smithay::wayland::shell::wlr_layer::Anchor;
use std::time::Duration;

use crate::animation::Animation;
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
    blocked_out_buffer: TextureBuffer<GlesTexture>,
    block_out_from: Option<BlockOutFrom>,
    geo_size: Size<f64, Logical>,
    pos: Point<f64, Logical>,
    buffer_offset: Point<f64, Logical>,
    blocked_out_buffer_offset: Point<f64, Logical>,
    transform_anim: Animation,
    opacity_anim: Animation,
    opacity_delay: Duration,
    config: niri_config::animations::LayerCloseAnim,
    start_alpha: f32,
    start_scale: f64,
    start_offset: Point<f64, Logical>,
    anchor: Anchor,
}

niri_render_elements! {
    ClosingLayerRenderElement => {
        Texture = RelocateRenderElement<RescaleRenderElement<PrimaryGpuTextureRenderElement>>,
    }
}

impl ClosingLayer {
    pub fn new<E: RenderElement<GlesRenderer>>(
        renderer: &mut GlesRenderer,
        snapshot: RenderSnapshot<E, E>,
        scale: Scale<f64>,
        mut geo_size: Size<f64, Logical>,
        pos: Point<f64, Logical>,
        transform_anim: Animation,
        opacity_anim: Animation,
        config: niri_config::animations::LayerCloseAnim,
        start: CloseAnimationStartState,
        anchor: Anchor,
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
        let (blocked_out_buffer, blocked_out_buffer_offset) =
            render_to_texture(snapshot.blocked_out_contents)
                .context("error rendering blocked-out contents")?;

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

        Ok(Self {
            buffer,
            blocked_out_buffer,
            block_out_from: snapshot.block_out_from,
            geo_size,
            pos,
            buffer_offset,
            blocked_out_buffer_offset,
            transform_anim,
            opacity_anim,
            opacity_delay: Duration::from_millis(u64::from(config.opacity_delay_ms)),
            config,
            start_alpha: start.start_alpha,
            start_scale: start.start_scale,
            start_offset: start.start_offset,
            anchor,
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

    pub fn render(
        &self,
        view_rect: Rectangle<f64, Logical>,
        scale: Scale<f64>,
        target: RenderTarget,
    ) -> ClosingLayerRenderElement {
        let (buffer, offset) = if target.should_block_out(self.block_out_from) {
            (&self.blocked_out_buffer, self.blocked_out_buffer_offset)
        } else {
            (&self.buffer, self.buffer_offset)
        };

        let transform_progress = self.transform_anim.clamped_value().clamp(0., 1.);
        let opacity_progress = self
            .opacity_anim
            .clamped_value_with_delay(self.opacity_delay)
            .clamp(0., 1.);
        let config = self.config;
        let target_alpha = config.opacity_to;
        let alpha = self.start_alpha + (target_alpha - self.start_alpha) * opacity_progress as f32;
        let target_scale = match config.style {
            niri_config::animations::LayerCloseAnimationStyle::Popout => config.scale_to,
            niri_config::animations::LayerCloseAnimationStyle::Fade
            | niri_config::animations::LayerCloseAnimationStyle::Slide
            | niri_config::animations::LayerCloseAnimationStyle::EdgeReveal => 1.,
        };
        let scale_factor =
            self.start_scale + (target_scale - self.start_scale) * transform_progress;
        let target_offset = match config.style {
            // slide is a full-surface translation. edge-reveal currently reuses the
            // same offset primitive with shorter configured distances, preserving a
            // separate style hook for later clipped reveal rendering.
            niri_config::animations::LayerCloseAnimationStyle::Slide => {
                edge_offset(config.edge, config.distance)
            }
            niri_config::animations::LayerCloseAnimationStyle::EdgeReveal => {
                edge_offset(config.edge, config.distance)
            }
            niri_config::animations::LayerCloseAnimationStyle::Fade
            | niri_config::animations::LayerCloseAnimationStyle::Popout => Point::from((0., 0.)),
        };
        let animation_offset = Point::new(
            self.start_offset.x + (target_offset.x - self.start_offset.x) * transform_progress,
            self.start_offset.y + (target_offset.y - self.start_offset.y) * transform_progress,
        );

        let elem = TextureRenderElement::from_texture_buffer(
            buffer.clone(),
            Point::from((0., 0.)),
            alpha.clamp(0., 1.),
            None,
            None,
            Kind::Unspecified,
        );
        let elem = PrimaryGpuTextureRenderElement(elem);

        let origin = match config.origin {
            niri_config::animations::LayerAnimationOrigin::Center => {
                self.geo_size.to_point().downscale(2.)
            }
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
        };
        let elem = RescaleRenderElement::from_element(
            elem,
            (origin - offset).to_physical_precise_round(scale),
            scale_factor.max(0.),
        );

        let mut location = self.pos + offset + animation_offset;
        location.x -= view_rect.loc.x;
        let elem = RelocateRenderElement::from_element(
            elem,
            location.to_physical_precise_round(scale),
            Relocate::Relative,
        );

        elem.into()
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
