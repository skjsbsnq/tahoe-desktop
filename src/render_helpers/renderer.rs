use std::cell::Cell;
use std::rc::Rc;

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::gles::{ffi, GlesFrame, GlesRenderer, GlesTexture};
use smithay::backend::renderer::{
    Bind, ExportMem, ImportAll, ImportMem, Offscreen, Renderer, RendererSuper, Texture,
};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Size};

use crate::backend::tty::{TtyFrame, TtyRenderer};

#[derive(Debug, Default)]
pub(crate) struct ScratchFramebuffer {
    id: Cell<ffi::types::GLuint>,
}

impl ScratchFramebuffer {
    fn for_user_data(data: &UserDataMap) -> Rc<Self> {
        Rc::clone(data.get_or_insert(|| Rc::new(Self::default())))
    }

    pub(crate) fn for_renderer(renderer: &GlesRenderer) -> Rc<Self> {
        Self::for_user_data(renderer.egl_context().user_data())
    }

    // The cache lives in EGL context user data. GL releases the FBO when that
    // context is destroyed, so steady-state rendering never needs Gen/Delete.
    pub(crate) unsafe fn get_or_create(&self, gl: &ffi::Gles2) -> ffi::types::GLuint {
        self.get_or_create_with(|| {
            let mut framebuffer = 0;
            unsafe { gl.GenFramebuffers(1, &mut framebuffer) };
            debug!(framebuffer, "created renderer-context scratch framebuffer");
            framebuffer
        })
    }

    fn get_or_create_with(
        &self,
        create: impl FnOnce() -> ffi::types::GLuint,
    ) -> ffi::types::GLuint {
        let framebuffer = self.id.get();
        if framebuffer != 0 {
            return framebuffer;
        }

        let framebuffer = create();
        self.id.set(framebuffer);
        framebuffer
    }
}

pub(crate) fn texture_cache_matches(
    actual_size: Size<i32, Buffer>,
    actual_format: Option<Fourcc>,
    expected_size: Size<i32, Buffer>,
    expected_format: Fourcc,
) -> bool {
    actual_size == expected_size && actual_format == Some(expected_format)
}

/// Trait with our main renderer requirements to save on the typing.
pub trait NiriRenderer:
    ImportAll
    + ImportMem
    + ExportMem
    + Bind<Dmabuf>
    + Offscreen<GlesTexture>
    + Renderer<TextureId = Self::NiriTextureId, Error = Self::NiriError>
    + AsGlesRenderer
{
    // Associated types to work around the instability of associated type bounds.
    type NiriTextureId: Texture + Clone + Send + 'static;
    type NiriError: std::error::Error
        + Send
        + Sync
        + From<<GlesRenderer as RendererSuper>::Error>
        + 'static;
}

impl<R> NiriRenderer for R
where
    R: ImportAll + ImportMem + ExportMem + Bind<Dmabuf> + Offscreen<GlesTexture> + AsGlesRenderer,
    R::TextureId: Texture + Clone + Send + 'static,
    R::Error:
        std::error::Error + Send + Sync + From<<GlesRenderer as RendererSuper>::Error> + 'static,
{
    type NiriTextureId = R::TextureId;
    type NiriError = R::Error;
}

/// Trait for getting the underlying `GlesRenderer`.
pub trait AsGlesRenderer {
    fn as_gles_renderer(&mut self) -> &mut GlesRenderer;
}

impl AsGlesRenderer for GlesRenderer {
    fn as_gles_renderer(&mut self) -> &mut GlesRenderer {
        self
    }
}

impl AsGlesRenderer for TtyRenderer<'_> {
    fn as_gles_renderer(&mut self) -> &mut GlesRenderer {
        self.as_mut()
    }
}

/// Trait for getting the underlying `GlesFrame`.
pub trait AsGlesFrame<'frame, 'buffer>
where
    Self: 'frame,
{
    fn as_gles_frame(&mut self) -> &mut GlesFrame<'frame, 'buffer>;
}

impl<'frame, 'buffer> AsGlesFrame<'frame, 'buffer> for GlesFrame<'frame, 'buffer> {
    fn as_gles_frame(&mut self) -> &mut GlesFrame<'frame, 'buffer> {
        self
    }
}

impl<'frame, 'buffer> AsGlesFrame<'frame, 'buffer> for TtyFrame<'_, 'frame, 'buffer> {
    fn as_gles_frame(&mut self) -> &mut GlesFrame<'frame, 'buffer> {
        self.as_mut()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use smithay::backend::allocator::Fourcc;
    use smithay::utils::user_data::UserDataMap;
    use smithay::utils::Size;

    use super::{texture_cache_matches, ScratchFramebuffer};

    #[test]
    fn scratch_framebuffer_is_created_once_per_context() {
        let data = UserDataMap::new();
        let cache = ScratchFramebuffer::for_user_data(&data);
        let creates = Cell::new(0);
        let create = || {
            creates.set(creates.get() + 1);
            41
        };

        assert_eq!(cache.get_or_create_with(create), 41);
        assert_eq!(cache.get_or_create_with(create), 41);
        assert_eq!(creates.get(), 1);
        assert!(Rc::ptr_eq(
            &cache,
            &ScratchFramebuffer::for_user_data(&data)
        ));
    }

    #[test]
    fn scratch_framebuffer_is_invalidated_with_renderer_context() {
        let first = ScratchFramebuffer::for_user_data(&UserDataMap::new());
        let second = ScratchFramebuffer::for_user_data(&UserDataMap::new());

        assert!(!Rc::ptr_eq(&first, &second));
        assert_eq!(first.get_or_create_with(|| 11), 11);
        assert_eq!(second.get_or_create_with(|| 29), 29);
    }

    #[test]
    fn reusable_texture_requires_matching_size_and_format() {
        let size = Size::from((1920, 1080));

        assert!(texture_cache_matches(
            size,
            Some(Fourcc::Abgr8888),
            size,
            Fourcc::Abgr8888
        ));
        assert!(!texture_cache_matches(
            Size::from((1280, 720)),
            Some(Fourcc::Abgr8888),
            size,
            Fourcc::Abgr8888
        ));
        assert!(!texture_cache_matches(
            size,
            Some(Fourcc::Argb8888),
            size,
            Fourcc::Abgr8888
        ));
    }
}
