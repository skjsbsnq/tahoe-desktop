use std::cmp::max;
use std::iter::{once, successors, zip};
use std::rc::Rc;

use anyhow::{ensure, Context as _};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::gles::{ffi, link_program, GlesError, GlesRenderer, GlesTexture};
use smithay::backend::renderer::{ContextId, Renderer as _, Texture as _};
use smithay::gpu_span_location;
use smithay::utils::{Buffer, Size};

use crate::render_helpers::renderer::{texture_cache_matches, ScratchFramebuffer};
use crate::render_helpers::shaders::Shaders;

#[derive(Debug)]
pub struct Blur {
    program: BlurProgram,
    /// Context ID of the renderer that created the program and the textures.
    renderer_context_id: ContextId<GlesTexture>,
    /// Output texture followed by intermediate textures, large to small.
    ///
    /// Created lazily and stored here to avoid recreating blur textures frequently.
    textures: Vec<GlesTexture>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct BlurOptions {
    pub passes: u8,
    pub offset: f64,
}

impl From<niri_config::Blur> for BlurOptions {
    fn from(config: niri_config::Blur) -> Self {
        Self {
            passes: config.passes,
            offset: config.offset,
        }
    }
}

fn texture_sizes(size: Size<i32, Buffer>, passes: u8) -> impl Iterator<Item = Size<i32, Buffer>> {
    successors(Some(size), |size| {
        Some(Size::new(max(1, size.w / 2), max(1, size.h / 2)))
    })
    .take(passes.clamp(1, 31) as usize + 1)
}

#[derive(Debug, Clone)]
pub struct BlurProgram(Rc<BlurProgramInner>);

#[derive(Debug)]
struct BlurProgramInner {
    down: BlurProgramInternal,
    up: BlurProgramInternal,
}

#[derive(Debug)]
struct BlurProgramInternal {
    program: ffi::types::GLuint,
    uniform_tex: ffi::types::GLint,
    uniform_half_pixel: ffi::types::GLint,
    uniform_offset: ffi::types::GLint,
    attrib_vert: ffi::types::GLint,
}

unsafe fn compile_program(gl: &ffi::Gles2, src: &str) -> Result<BlurProgramInternal, GlesError> {
    let program = unsafe { link_program(gl, include_str!("shaders/blur.vert"), src)? };

    let vert = c"vert";
    let tex = c"tex";
    let half_pixel = c"half_pixel";
    let offset = c"offset";

    Ok(BlurProgramInternal {
        program,
        uniform_tex: gl.GetUniformLocation(program, tex.as_ptr()),
        uniform_half_pixel: gl.GetUniformLocation(program, half_pixel.as_ptr()),
        uniform_offset: gl.GetUniformLocation(program, offset.as_ptr()),
        attrib_vert: gl.GetAttribLocation(program, vert.as_ptr()),
    })
}

impl BlurProgram {
    pub fn compile(renderer: &mut GlesRenderer) -> anyhow::Result<Self> {
        renderer
            .with_context(move |gl| unsafe {
                let down = compile_program(gl, include_str!("shaders/blur_down.frag"))
                    .context("error compiling blur_down shader")?;
                let up = compile_program(gl, include_str!("shaders/blur_up.frag"))
                    .context("error compiling blur_up shader")?;
                Ok(Self(Rc::new(BlurProgramInner { down, up })))
            })
            .context("error making GL context current")?
    }

    pub fn destroy(self, renderer: &mut GlesRenderer) -> Result<(), GlesError> {
        renderer.with_context(move |gl| unsafe {
            gl.DeleteProgram(self.0.down.program);
            gl.DeleteProgram(self.0.up.program);
        })
    }
}

impl Blur {
    pub fn new(renderer: &mut GlesRenderer) -> Option<Self> {
        let program = Shaders::get(renderer).blur.clone()?;
        Some(Self {
            program,
            renderer_context_id: renderer.context_id(),
            textures: Vec::new(),
        })
    }

    pub fn context_id(&self) -> ContextId<GlesTexture> {
        self.renderer_context_id.clone()
    }

    pub fn prepare_textures(
        &mut self,
        mut create_texture: impl FnMut(Fourcc, Size<i32, Buffer>) -> Result<GlesTexture, GlesError>,
        source: &GlesTexture,
        options: BlurOptions,
    ) -> anyhow::Result<()> {
        let _span = tracy_client::span!("Blur::prepare_textures");

        let passes = options.passes.clamp(1, 31) as usize;
        let size = source.size();

        // Reuse the complete texture pyramid while size, format and pass count stay compatible.
        for (i, size) in texture_sizes(size, options.passes).enumerate() {
            if let Some(texture) = self.textures.get_mut(i) {
                let actual_size = texture.size();
                let actual_format = texture.format();
                let size_changed = actual_size != size;
                let format_changed = actual_format != Some(Fourcc::Abgr8888);
                let output_is_shared = i == 0 && !texture.is_unique_reference();
                let texture_matches =
                    texture_cache_matches(actual_size, actual_format, size, Fourcc::Abgr8888);
                if !texture_matches || output_is_shared {
                    debug!(
                        step = i,
                        size_changed,
                        format_changed,
                        output_is_shared,
                        "recreating incompatible blur textures"
                    );
                    self.textures.truncate(i);
                }
            }

            if self.textures.len() == i {
                let texture: GlesTexture =
                    create_texture(Fourcc::Abgr8888, size).context("error creating texture")?;
                self.textures.push(texture);
            }
        }

        // Drop any no longer needed textures.
        self.textures.truncate(passes + 1);

        Ok(())
    }

    pub fn render(
        &mut self,
        renderer: &mut GlesRenderer,
        source: &GlesTexture,
        options: BlurOptions,
    ) -> anyhow::Result<GlesTexture> {
        let _span = tracy_client::span!("Blur::render");
        trace!("rendering blur");

        ensure!(
            renderer.context_id() == self.renderer_context_id,
            "wrong renderer"
        );

        let passes = options.passes.clamp(1, 31) as usize;
        let size = source.size();

        ensure!(
            self.textures.len() == passes + 1,
            "wrong textures len: expected {}, got {}",
            passes + 1,
            self.textures.len()
        );

        let output = &mut self.textures[0];
        ensure!(
            output.size() == size,
            "wrong output texture size: expected {size:?}, got {:?}",
            output.size()
        );

        ensure!(
            output.is_unique_reference(),
            "output texture has a non-unique reference"
        );

        let scratch_framebuffer = ScratchFramebuffer::for_renderer(renderer);
        renderer.with_profiled_context(gpu_span_location!("Blur::render"), |gl| unsafe {
            while gl.GetError() != ffi::NO_ERROR {}

            gl.Disable(ffi::BLEND);
            gl.Disable(ffi::SCISSOR_TEST);

            gl.ActiveTexture(ffi::TEXTURE0);

            let framebuffer = scratch_framebuffer.get_or_create(gl);
            gl.BindFramebuffer(ffi::FRAMEBUFFER, framebuffer);

            let program = &self.program.0.down;
            gl.UseProgram(program.program);
            gl.Uniform1i(program.uniform_tex, 0);
            gl.Uniform1f(program.uniform_offset, options.offset as f32);

            let vertices: [f32; 12] = [0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0.0];
            gl.EnableVertexAttribArray(program.attrib_vert as u32);
            gl.BindBuffer(ffi::ARRAY_BUFFER, 0);
            gl.VertexAttribPointer(
                program.attrib_vert as u32,
                2,
                ffi::FLOAT,
                ffi::FALSE,
                0,
                vertices.as_ptr().cast(),
            );

            let src = once(source).chain(&self.textures[1..]);
            let dst = &self.textures[1..];
            for (src, dst) in zip(src, dst) {
                let dst_size = dst.size();
                let w = dst_size.w;
                let h = dst_size.h;
                gl.Viewport(0, 0, w, h);

                // During downsampling, half_pixel is half of the destination pixel.
                gl.Uniform2f(program.uniform_half_pixel, 0.5 / w as f32, 0.5 / h as f32);

                let src = src.tex_id();
                let dst = dst.tex_id();

                trace!("drawing down {src} to {dst}");
                gl.FramebufferTexture2D(
                    ffi::FRAMEBUFFER,
                    ffi::COLOR_ATTACHMENT0,
                    ffi::TEXTURE_2D,
                    dst,
                    0,
                );

                gl.BindTexture(ffi::TEXTURE_2D, src);
                gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
                gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
                gl.TexParameteri(
                    ffi::TEXTURE_2D,
                    ffi::TEXTURE_WRAP_S,
                    ffi::CLAMP_TO_EDGE as i32,
                );
                gl.TexParameteri(
                    ffi::TEXTURE_2D,
                    ffi::TEXTURE_WRAP_T,
                    ffi::CLAMP_TO_EDGE as i32,
                );

                gl.DrawArrays(ffi::TRIANGLES, 0, 6);
            }

            gl.DisableVertexAttribArray(program.attrib_vert as u32);

            // Up
            let program = &self.program.0.up;
            gl.UseProgram(program.program);
            gl.Uniform1i(program.uniform_tex, 0);
            gl.Uniform1f(program.uniform_offset, options.offset as f32);

            let vertices: [f32; 12] = [0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0.0];
            gl.EnableVertexAttribArray(program.attrib_vert as u32);
            gl.BindBuffer(ffi::ARRAY_BUFFER, 0);
            gl.VertexAttribPointer(
                program.attrib_vert as u32,
                2,
                ffi::FLOAT,
                ffi::FALSE,
                0,
                vertices.as_ptr().cast(),
            );

            let src = self.textures.iter().rev();
            let dst = self.textures.iter().rev().skip(1);
            for (src, dst) in zip(src, dst) {
                let dst_size = dst.size();
                let w = dst_size.w;
                let h = dst_size.h;
                gl.Viewport(0, 0, w, h);

                // During upsampling, half_pixel is half of the source pixel.
                let src_size = src.size();
                let src_w = src_size.w as f32;
                let src_h = src_size.h as f32;
                gl.Uniform2f(program.uniform_half_pixel, 0.5 / src_w, 0.5 / src_h);

                let src = src.tex_id();
                let dst = dst.tex_id();

                trace!("drawing up {src} to {dst}");
                gl.FramebufferTexture2D(
                    ffi::FRAMEBUFFER,
                    ffi::COLOR_ATTACHMENT0,
                    ffi::TEXTURE_2D,
                    dst,
                    0,
                );

                gl.BindTexture(ffi::TEXTURE_2D, src);
                gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
                gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
                gl.TexParameteri(
                    ffi::TEXTURE_2D,
                    ffi::TEXTURE_WRAP_S,
                    ffi::CLAMP_TO_EDGE as i32,
                );
                gl.TexParameteri(
                    ffi::TEXTURE_2D,
                    ffi::TEXTURE_WRAP_T,
                    ffi::CLAMP_TO_EDGE as i32,
                );

                gl.DrawArrays(ffi::TRIANGLES, 0, 6);
            }

            gl.DisableVertexAttribArray(program.attrib_vert as u32);

            gl.FramebufferTexture2D(
                ffi::FRAMEBUFFER,
                ffi::COLOR_ATTACHMENT0,
                ffi::TEXTURE_2D,
                0,
                0,
            );
            gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
        })?;

        Ok(self.textures[0].clone())
    }
}

#[cfg(test)]
mod tests {
    use smithay::utils::Size;

    use super::texture_sizes;

    #[test]
    fn texture_pyramid_tracks_size_and_pass_count() {
        let sizes: Vec<_> = texture_sizes(Size::from((1920, 1080)), 4).collect();

        assert_eq!(
            sizes,
            vec![
                Size::from((1920, 1080)),
                Size::from((960, 540)),
                Size::from((480, 270)),
                Size::from((240, 135)),
                Size::from((120, 67)),
            ]
        );
    }

    #[test]
    fn texture_pyramid_clamps_passes_and_dimensions() {
        assert_eq!(texture_sizes(Size::from((1, 1)), 0).count(), 2);

        let sizes: Vec<_> = texture_sizes(Size::from((2, 3)), u8::MAX).collect();
        assert_eq!(sizes.len(), 32);
        assert_eq!(sizes.last(), Some(&Size::from((1, 1))));
    }
}
