use std::cmp::min;
use std::collections::HashMap;
use std::ffi::CString;
use std::fmt;
use std::fmt::Write as _;
use std::fs::File;
use std::io::Seek as _;
use std::io::Write as _;
use std::os::unix::io::{AsFd as _, FromRawFd as _};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::protocols::raw::tahoe_glass::v1::client::tahoe_glass_manager_v1::TahoeGlassManagerV1;
use crate::protocols::raw::tahoe_glass::v1::client::tahoe_glass_surface_v1::TahoeGlassSurfaceV1;
use calloop::EventLoop;
use calloop_wayland_source::WaylandSource;
use single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1;
use smithay::reexports::wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_locked_pointer_v1::ZwpLockedPointerV1,
    zwp_pointer_constraints_v1::{self, ZwpPointerConstraintsV1},
};
use smithay::reexports::wayland_protocols::wp::single_pixel_buffer;
use smithay::reexports::wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use smithay::reexports::wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use smithay::reexports::wayland_protocols::xdg::shell::client::xdg_positioner::{
    self, XdgPositioner,
};
use smithay::reexports::wayland_protocols::xdg::shell::client::xdg_popup::{self, XdgPopup};
use smithay::reexports::wayland_protocols::xdg::shell::client::xdg_surface::{self, XdgSurface};
use smithay::reexports::wayland_protocols::xdg::shell::client::xdg_toplevel::{self, XdgToplevel};
use smithay::reexports::wayland_protocols::xdg::shell::client::xdg_wm_base::{self, XdgWmBase};
use smithay::reexports::wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};
use smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::{
    self, ZwlrLayerShellV1,
};
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{
    self, ZwlrLayerSurfaceV1,
};
use wayland_backend::client::Backend;
use wayland_client::globals::Global;
use wayland_client::protocol::wl_buffer::{self, WlBuffer};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_display::WlDisplay;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_pointer::WlPointer;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_subcompositor::WlSubcompositor;
use wayland_client::protocol::wl_subsurface::WlSubsurface;
use wayland_client::protocol::wl_surface::{self, WlSurface};
use wayland_client::{Connection, Dispatch, Proxy as _, QueueHandle};

use crate::utils::id::IdCounter;

pub struct Client {
    pub id: ClientId,
    pub event_loop: EventLoop<'static, State>,
    pub connection: Connection,
    pub qh: QueueHandle<State>,
    pub display: WlDisplay,
    pub state: State,
}

pub struct State {
    pub qh: QueueHandle<State>,

    pub globals: Vec<Global>,
    pub outputs: HashMap<WlOutput, String>,

    pub compositor: Option<WlCompositor>,
    pub subcompositor: Option<WlSubcompositor>,
    pub seat: Option<WlSeat>,
    pub pointer: Option<WlPointer>,
    pub pointer_constraints: Option<ZwpPointerConstraintsV1>,
    pub xdg_wm_base: Option<XdgWmBase>,
    pub layer_shell: Option<ZwlrLayerShellV1>,
    pub foreign_toplevel_manager: Option<ZwlrForeignToplevelManagerV1>,
    pub ext_foreign_toplevel_list: Option<ExtForeignToplevelListV1>,
    pub tahoe_glass_manager: Option<TahoeGlassManagerV1>,
    pub spbm: Option<WpSinglePixelBufferManagerV1>,
    pub viewporter: Option<WpViewporter>,
    pub shm: Option<WlShm>,

    pub windows: Vec<Window>,
    pub layers: Vec<LayerSurface>,
    pub locked_pointers: Vec<ZwpLockedPointerV1>,
    /// Keep-alive for subsurface proxies created by tests.
    pub subsurfaces: Vec<WlSubsurface>,
    /// Popups created by tests.
    pub popups: Vec<TestPopup>,
    /// Keep-alive fds for wl_shm pools: the fd must stay open until the
    /// compositor processed `create_pool` (it duplicates it server-side).
    pub pool_files: Vec<File>,
    pub foreign_toplevels: Vec<ZwlrForeignToplevelHandleV1>,
    /// Creation-order records for coordinated ext ↔ wlr pairing tests (R11).
    pub wlr_foreign_toplevel_meta: Vec<WlrForeignToplevelMeta>,
    pub ext_foreign_toplevels: Vec<ExtForeignToplevelMeta>,
}

/// Client-side snapshot of one wlr foreign-toplevel handle stream event set.
#[derive(Debug, Clone)]
pub struct WlrForeignToplevelMeta {
    pub handle: ZwlrForeignToplevelHandleV1,
    pub title: Option<String>,
    pub app_id: Option<String>,
    pub done: bool,
    pub closed: bool,
}

/// Client-side snapshot of one ext-foreign-toplevel-list handle.
#[derive(Debug, Clone)]
pub struct ExtForeignToplevelMeta {
    pub handle: ExtForeignToplevelHandleV1,
    pub identifier: Option<String>,
    pub title: Option<String>,
    pub app_id: Option<String>,
    pub done: bool,
    pub closed: bool,
}

/// Client-side xdg popup snapshot: proxies plus the last configure the
/// compositor sent.
#[derive(Debug, Clone)]
pub struct TestPopup {
    pub surface: WlSurface,
    pub xdg_surface: XdgSurface,
    /// Keep-alive: the popup proxy must stay alive for the popup to stay
    /// mapped; tests access the popup through [`Self::surface`].
    #[allow(dead_code)]
    pub popup: XdgPopup,
    pub configure_serial: Option<u32>,
}

pub struct Window {
    pub qh: QueueHandle<State>,
    pub spbm: WpSinglePixelBufferManagerV1,

    pub surface: WlSurface,
    pub xdg_surface: XdgSurface,
    pub xdg_toplevel: XdgToplevel,
    pub viewport: WpViewport,
    pub pending_configure: Configure,
    pub configures_received: Vec<(u32, Configure)>,
    pub wm_capabilities: Vec<xdg_toplevel::WmCapabilities>,
    pub close_requested: bool,

    pub configures_looked_at: usize,
}

pub struct LayerSurface {
    pub qh: QueueHandle<State>,
    pub spbm: WpSinglePixelBufferManagerV1,

    pub surface: WlSurface,
    pub layer_surface: ZwlrLayerSurfaceV1,
    pub viewport: WpViewport,
    pub configures_received: Vec<(u32, LayerConfigure)>,
    pub close_requested: bool,

    pub configures_looked_at: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Configure {
    pub size: (i32, i32),
    pub bounds: Option<(i32, i32)>,
    pub states: Vec<xdg_toplevel::State>,
}

#[derive(Debug, Clone, Copy)]
pub struct LayerConfigure {
    pub size: (u32, u32),
}

#[derive(Clone, Copy, Default)]
pub struct LayerMargin {
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub left: i32,
}

#[derive(Clone, Copy, Default)]
pub struct LayerConfigureProps {
    pub size: Option<(u32, u32)>,
    pub anchor: Option<zwlr_layer_surface_v1::Anchor>,
    pub exclusive_zone: Option<i32>,
    pub margin: Option<LayerMargin>,
    pub kb_interactivity: Option<zwlr_layer_surface_v1::KeyboardInteractivity>,
    pub layer: Option<zwlr_layer_shell_v1::Layer>,
    pub exclusive_edge: Option<zwlr_layer_surface_v1::Anchor>,
}

#[derive(Default)]
pub struct SyncData {
    pub done: AtomicBool,
}

static CLIENT_ID_COUNTER: IdCounter = IdCounter::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(u64);

impl ClientId {
    fn next() -> ClientId {
        ClientId(CLIENT_ID_COUNTER.next())
    }
}

impl fmt::Display for Configure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "size: {} × {}, ", self.size.0, self.size.1)?;
        if let Some(bounds) = self.bounds {
            write!(f, "bounds: {} × {}, ", bounds.0, bounds.1)?;
        } else {
            write!(f, "bounds: none, ")?;
        }
        write!(f, "states: {:?}", self.states)?;
        Ok(())
    }
}

impl fmt::Display for LayerConfigure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "size: {} × {}", self.size.0, self.size.1)?;
        Ok(())
    }
}

impl Client {
    pub fn new(stream: UnixStream) -> Self {
        let id = ClientId::next();

        let event_loop = EventLoop::try_new().unwrap();
        let backend = Backend::connect(stream).unwrap();
        let connection = Connection::from_backend(backend);
        let queue = connection.new_event_queue();
        let qh = queue.handle();
        WaylandSource::new(connection.clone(), queue)
            .insert(event_loop.handle())
            .unwrap();

        let display = connection.display();
        let _registry = display.get_registry(&qh, ());
        connection.flush().unwrap();

        let state = State {
            qh: qh.clone(),
            globals: Vec::new(),
            outputs: HashMap::new(),
            compositor: None,
            subcompositor: None,
            seat: None,
            pointer: None,
            pointer_constraints: None,
            xdg_wm_base: None,
            layer_shell: None,
            foreign_toplevel_manager: None,
            ext_foreign_toplevel_list: None,
            tahoe_glass_manager: None,
            spbm: None,
            viewporter: None,
            shm: None,
            pool_files: Vec::new(),
            windows: Vec::new(),
            layers: Vec::new(),
            locked_pointers: Vec::new(),
            subsurfaces: Vec::new(),
            popups: Vec::new(),
            foreign_toplevels: Vec::new(),
            wlr_foreign_toplevel_meta: Vec::new(),
            ext_foreign_toplevels: Vec::new(),
        };

        Self {
            id,
            event_loop,
            connection,
            qh,
            display,
            state,
        }
    }

    pub fn dispatch(&mut self) {
        self.event_loop
            .dispatch(Duration::ZERO, &mut self.state)
            .unwrap();

        if let Some(error) = self.connection.protocol_error() {
            panic!("{error}");
        }
    }

    pub fn send_sync(&self) -> Arc<SyncData> {
        let data = Arc::new(SyncData::default());
        self.display.sync(&self.qh, data.clone());
        self.connection.flush().unwrap();
        data
    }

    pub fn create_window(&mut self) -> &mut Window {
        self.state.create_window()
    }

    pub fn window(&mut self, surface: &WlSurface) -> &mut Window {
        self.state.window(surface)
    }

    pub fn create_layer(
        &mut self,
        output: Option<&WlOutput>,
        layer: zwlr_layer_shell_v1::Layer,
        namespace: &str,
    ) -> &mut LayerSurface {
        self.state.create_layer(output, layer, namespace.to_owned())
    }

    pub fn layer(&mut self, surface: &WlSurface) -> &mut LayerSurface {
        self.state.layer(surface)
    }

    pub fn destroy_layer(&mut self, surface: &WlSurface) -> WpViewport {
        self.state.destroy_layer(surface)
    }

    pub fn create_layer_on_surface(
        &mut self,
        surface: &WlSurface,
        output: Option<&WlOutput>,
        layer: zwlr_layer_shell_v1::Layer,
        namespace: &str,
        viewport: &WpViewport,
    ) -> &mut LayerSurface {
        self.state
            .create_layer_on_surface(surface, output, layer, namespace.to_owned(), viewport)
    }

    pub fn foreign_toplevel(&self, idx: usize) -> ZwlrForeignToplevelHandleV1 {
        self.state.foreign_toplevels[idx].clone()
    }

    /// Live ext-list handles that have received `done` and are not closed.
    pub fn ext_foreign_toplevel_ready(&self) -> Vec<&ExtForeignToplevelMeta> {
        self.state
            .ext_foreign_toplevels
            .iter()
            .filter(|m| m.done && !m.closed)
            .collect()
    }

    /// Live wlr handles that have received `done` and are not closed.
    pub fn wlr_foreign_toplevel_ready(&self) -> Vec<&WlrForeignToplevelMeta> {
        self.state
            .wlr_foreign_toplevel_meta
            .iter()
            .filter(|m| m.done && !m.closed)
            .collect()
    }

    /// FIFO coordinated pairing used by R11: both managers share niri
    /// `ToplevelData` creation order (ext then wlr per window / same HashMap
    /// walk on bind). Returns (identifier, app_id, title) triples for pairs.
    pub fn pair_ext_wlr_by_creation_order(
        &self,
    ) -> Result<Vec<(String, Option<String>, Option<String>)>, String> {
        let ext: Vec<_> = self.ext_foreign_toplevel_ready();
        let wlr: Vec<_> = self.wlr_foreign_toplevel_ready();
        if ext.len() != wlr.len() {
            return Err(format!(
                "ext/wlr ready count desync: ext={} wlr={}",
                ext.len(),
                wlr.len()
            ));
        }
        let mut pairs = Vec::with_capacity(ext.len());
        for (e, w) in ext.iter().zip(wlr.iter()) {
            let Some(identifier) = e.identifier.clone() else {
                return Err("ext handle missing identifier".into());
            };
            if identifier.is_empty() {
                return Err("ext identifier empty".into());
            }
            // Fail closed on app_id mismatch when both sides published one.
            match (&e.app_id, &w.app_id) {
                (Some(a), Some(b)) if a != b => {
                    return Err(format!(
                        "app_id desync on pair identifier={identifier}: ext={a:?} wlr={b:?}"
                    ));
                }
                _ => {}
            }
            pairs.push((identifier, e.app_id.clone(), e.title.clone()));
        }
        Ok(pairs)
    }

    pub fn tahoe_glass_manager(&self) -> TahoeGlassManagerV1 {
        self.state
            .tahoe_glass_manager
            .clone()
            .expect("tahoe_glass_manager global not bound")
    }

    pub fn output(&mut self, name: &str) -> WlOutput {
        self.state
            .outputs
            .iter()
            .find(|(_, v)| *v == name)
            .unwrap()
            .0
            .clone()
    }
}

impl State {
    /// Lock the pointer on `surface` (one-shot lifetime) through the real
    /// pointer-constraints protocol, so `PointerConstraintsHandler::
    /// cursor_position_hint` can be driven by a real client commit.
    pub fn lock_pointer(&mut self, surface: &WlSurface) -> ZwpLockedPointerV1 {
        let constraints = self
            .pointer_constraints
            .as_ref()
            .expect("zwp_pointer_constraints_v1 global not bound");
        let seat = self.seat.as_ref().expect("wl_seat global not bound");
        let pointer = self
            .pointer
            .get_or_insert_with(|| seat.get_pointer(&self.qh, ()));
        let locked = constraints.lock_pointer(
            surface,
            pointer,
            None,
            zwp_pointer_constraints_v1::Lifetime::Oneshot,
            &self.qh,
            (),
        );
        self.locked_pointers.push(locked.clone());
        locked
    }

    pub fn create_window(&mut self) -> &mut Window {
        let compositor = self.compositor.as_ref().unwrap();
        let xdg_wm_base = self.xdg_wm_base.as_ref().unwrap();
        let viewporter = self.viewporter.as_ref().unwrap();

        let surface = compositor.create_surface(&self.qh, ());
        let xdg_surface = xdg_wm_base.get_xdg_surface(&surface, &self.qh, ());
        let xdg_toplevel = xdg_surface.get_toplevel(&self.qh, ());
        let viewport = viewporter.get_viewport(&surface, &self.qh, ());

        let window = Window {
            qh: self.qh.clone(),
            spbm: self.spbm.clone().unwrap(),

            surface,
            xdg_surface,
            xdg_toplevel,
            viewport,
            pending_configure: Configure::default(),
            configures_received: Vec::new(),
            wm_capabilities: Vec::new(),
            close_requested: false,

            configures_looked_at: 0,
        };

        self.windows.push(window);
        self.windows.last_mut().unwrap()
    }

    pub fn window(&mut self, surface: &WlSurface) -> &mut Window {
        self.windows
            .iter_mut()
            .find(|w| w.surface == *surface)
            .unwrap()
    }

    /// Attach a real wl_shm ARGB8888 buffer with the given pixel data to a
    /// surface.
    ///
    /// `pixels` must be `width * height * 4` bytes in wl_shm ARGB8888 memory
    /// order (`B, G, R, A` per pixel in little-endian memory). The pool fd is
    /// kept alive in [`Self::pool_files`] until the compositor processed the
    /// `create_pool` request.
    pub fn attach_shm_buffer(
        &mut self,
        surface: &WlSurface,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) {
        let shm = self.shm.as_ref().expect("wl_shm global not bound");
        let name = CString::new("niri-test-shm-pool").unwrap();
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0, "memfd_create failed");
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(pixels).expect("error writing shm pool");
        file.seek(std::io::SeekFrom::Start(0))
            .expect("error rewinding shm pool");

        let pool = shm.create_pool(file.as_fd(), pixels.len() as i32, &self.qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            (width * 4) as i32,
            wl_shm::Format::Argb8888,
            &self.qh,
            (),
        );
        self.pool_files.push(file);
        surface.attach(Some(&buffer), 0, 0);
        // Damage the whole surface like a real client that visibly updates its
        // content (the compositor's content version advances on damaged
        // commits).
        surface.damage(0, 0, width as i32, height as i32);
    }

    /// Create a desynced subsurface of `parent` (kept alive in
    /// [`Self::subsurfaces`]) and return its surface.
    pub fn create_subsurface(&mut self, parent: &WlSurface) -> WlSurface {
        let subcompositor = self
            .subcompositor
            .as_ref()
            .expect("wl_subcompositor not bound");
        let compositor = self.compositor.as_ref().expect("wl_compositor not bound");
        let surface = compositor.create_surface(&self.qh, ());
        let subsurface = subcompositor.get_subsurface(&surface, parent, &self.qh, ());
        subsurface.set_desync();
        self.subsurfaces.push(subsurface);
        surface
    }

    /// Create an xdg popup of the given parent xdg surface (kept alive in
    /// [`Self::popups`]) and return its surface.
    ///
    /// The popup is not committed yet: the caller must commit once (to get
    /// the initial configure), ack it, then attach a buffer and commit again
    /// to map the popup.
    pub fn create_popup(
        &mut self,
        parent: &XdgSurface,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> WlSurface {
        let compositor = self.compositor.as_ref().expect("wl_compositor not bound");
        let xdg_wm_base = self.xdg_wm_base.as_ref().expect("xdg_wm_base not bound");
        let surface = compositor.create_surface(&self.qh, ());
        let xdg_surface = xdg_wm_base.get_xdg_surface(&surface, &self.qh, ());
        let positioner = xdg_wm_base.create_positioner(&self.qh, ());
        positioner.set_size(width as i32, height as i32);
        positioner.set_anchor_rect(x, y, 1, 1);
        positioner.set_anchor(xdg_positioner::Anchor::TopLeft);
        positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
        let popup = xdg_surface.get_popup(Some(parent), &positioner, &self.qh, ());
        self.popups.push(TestPopup {
            surface: surface.clone(),
            xdg_surface: xdg_surface.clone(),
            popup,
            configure_serial: None,
        });
        surface
    }

    pub fn create_layer(
        &mut self,
        output: Option<&WlOutput>,
        layer: zwlr_layer_shell_v1::Layer,
        namespace: String,
    ) -> &mut LayerSurface {
        let compositor = self.compositor.as_ref().unwrap();
        let layer_shell = self.layer_shell.as_ref().unwrap();
        let viewporter = self.viewporter.as_ref().unwrap();

        let surface = compositor.create_surface(&self.qh, ());
        let layer_surface =
            layer_shell.get_layer_surface(&surface, output, layer, namespace, &self.qh, ());
        let viewport = viewporter.get_viewport(&surface, &self.qh, ());

        let layer_surface = LayerSurface {
            qh: self.qh.clone(),
            spbm: self.spbm.clone().unwrap(),

            surface,
            layer_surface,
            viewport,
            configures_received: Vec::new(),
            close_requested: false,

            configures_looked_at: 0,
        };

        self.layers.push(layer_surface);
        self.layers.last_mut().unwrap()
    }

    pub fn layer(&mut self, surface: &WlSurface) -> &mut LayerSurface {
        self.layers
            .iter_mut()
            .find(|w| w.surface == *surface)
            .unwrap()
    }

    /// Drop the client-side bookkeeping for a layer surface whose
    /// `zwlr_layer_surface_v1` object the test already destroyed, leaving the
    /// `wl_surface` alive so it can take a new role binding. Returns the
    /// surface's viewport proxy: a `wl_surface` may only have one viewport
    /// object for its lifetime, so a re-bound layer surface must reuse it.
    pub fn destroy_layer(&mut self, surface: &WlSurface) -> WpViewport {
        let idx = self
            .layers
            .iter()
            .position(|w| w.surface == *surface)
            .expect("destroy_layer: unknown layer surface");
        let layer = self.layers.remove(idx);
        layer.viewport.clone()
    }

    /// Create a new layer surface object on an existing `wl_surface` (the
    /// previous layer surface on it must have been destroyed first), reusing
    /// the surface's existing viewport proxy.
    pub fn create_layer_on_surface(
        &mut self,
        surface: &WlSurface,
        output: Option<&WlOutput>,
        layer: zwlr_layer_shell_v1::Layer,
        namespace: String,
        viewport: &WpViewport,
    ) -> &mut LayerSurface {
        let layer_shell = self.layer_shell.as_ref().unwrap();

        let layer_surface =
            layer_shell.get_layer_surface(surface, output, layer, namespace, &self.qh, ());

        let layer_surface = LayerSurface {
            qh: self.qh.clone(),
            spbm: self.spbm.clone().unwrap(),

            surface: surface.clone(),
            layer_surface,
            viewport: viewport.clone(),
            configures_received: Vec::new(),
            close_requested: false,

            configures_looked_at: 0,
        };

        self.layers.push(layer_surface);
        self.layers.last_mut().unwrap()
    }
}

impl Window {
    pub fn commit(&self) {
        self.surface.commit();
    }

    pub fn ack_last(&self) {
        let serial = self.configures_received.last().unwrap().0;
        self.xdg_surface.ack_configure(serial);
    }

    /// Ack a specific configure serial (for old-vs-latest serial fixture coverage).
    pub fn ack_serial(&self, serial: u32) {
        self.xdg_surface.ack_configure(serial);
    }

    pub fn ack_last_and_commit(&self) {
        self.ack_last();
        self.commit();
    }

    pub fn attach_new_buffer(&self) {
        let buffer = self.spbm.create_u32_rgba_buffer(0, 0, 0, 0, &self.qh, ());
        self.surface.attach(Some(&buffer), 0, 0);
    }

    pub fn attach_null(&self) {
        self.surface.attach(None, 0, 0);
    }

    pub fn set_size(&self, w: u16, h: u16) {
        self.viewport.set_destination(i32::from(w), i32::from(h));
    }

    pub fn set_fullscreen(&self, output: Option<&WlOutput>) {
        self.xdg_toplevel.set_fullscreen(output);
    }

    pub fn unset_fullscreen(&self) {
        self.xdg_toplevel.unset_fullscreen();
    }

    pub fn set_maximized(&self) {
        self.xdg_toplevel.set_maximized();
    }

    pub fn unset_maximized(&self) {
        self.xdg_toplevel.unset_maximized();
    }

    pub fn set_minimized(&self) {
        self.xdg_toplevel.set_minimized();
    }

    pub fn set_parent(&self, parent: Option<&XdgToplevel>) {
        self.xdg_toplevel.set_parent(parent);
    }

    pub fn set_title(&self, title: &str) {
        self.xdg_toplevel.set_title(title.to_owned());
    }

    pub fn set_app_id(&self, app_id: &str) {
        self.xdg_toplevel.set_app_id(app_id.to_owned());
    }

    pub fn recent_configures(&mut self) -> impl Iterator<Item = &Configure> {
        let start = self.configures_looked_at;
        self.configures_looked_at = self.configures_received.len();
        self.configures_received[start..].iter().map(|(_, c)| c)
    }

    pub fn format_recent_configures(&mut self) -> String {
        let mut buf = String::new();
        for configure in self.recent_configures() {
            if !buf.is_empty() {
                buf.push('\n');
            }
            write!(buf, "{configure}").unwrap();
        }
        buf
    }
}

impl LayerSurface {
    pub fn commit(&self) {
        self.surface.commit();
    }

    pub fn ack_last(&self) {
        let serial = self.configures_received.last().unwrap().0;
        self.layer_surface.ack_configure(serial);
    }

    pub fn ack_last_and_commit(&self) {
        self.ack_last();
        self.commit();
    }

    pub fn set_configure_props(&self, props: LayerConfigureProps) {
        let LayerConfigureProps {
            size,
            anchor,
            exclusive_zone,
            margin,
            kb_interactivity,
            layer,
            exclusive_edge,
        } = props;

        if let Some(x) = size {
            self.layer_surface.set_size(x.0, x.1);
        }
        if let Some(x) = anchor {
            self.layer_surface.set_anchor(x);
        }
        if let Some(x) = exclusive_zone {
            self.layer_surface.set_exclusive_zone(x);
        }
        if let Some(x) = margin {
            self.layer_surface
                .set_margin(x.top, x.right, x.bottom, x.left);
        }
        if let Some(x) = kb_interactivity {
            self.layer_surface.set_keyboard_interactivity(x);
        }
        if let Some(x) = layer {
            self.layer_surface.set_layer(x);
        }
        if let Some(x) = exclusive_edge {
            self.layer_surface.set_exclusive_edge(x);
        }
    }

    pub fn attach_new_buffer(&self) {
        let buffer = self.spbm.create_u32_rgba_buffer(0, 0, 0, 0, &self.qh, ());
        self.surface.attach(Some(&buffer), 0, 0);
    }

    pub fn attach_null(&self) {
        self.surface.attach(None, 0, 0);
    }

    pub fn set_size(&self, w: u16, h: u16) {
        self.viewport.set_destination(i32::from(w), i32::from(h));
    }

    pub fn recent_configures(&mut self) -> impl Iterator<Item = &LayerConfigure> {
        let start = self.configures_looked_at;
        self.configures_looked_at = self.configures_received.len();
        self.configures_received[start..].iter().map(|(_, c)| c)
    }

    pub fn format_recent_configures(&mut self) -> String {
        let mut buf = String::new();
        for configure in self.recent_configures() {
            if !buf.is_empty() {
                buf.push('\n');
            }
            write!(buf, "{configure}").unwrap();
        }
        buf
    }
}

impl Dispatch<WlCallback, Arc<SyncData>> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlCallback,
        event: <WlCallback as wayland_client::Proxy>::Event,
        data: &Arc<SyncData>,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_callback::Event::Done { .. } => data.done.store(true, Ordering::Relaxed),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: <WlRegistry as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == WlCompositor::interface().name {
                    let version = min(version, WlCompositor::interface().version);
                    state.compositor = Some(registry.bind(name, version, qh, ()));
                } else if interface == WlSubcompositor::interface().name {
                    let version = min(version, WlSubcompositor::interface().version);
                    state.subcompositor = Some(registry.bind(name, version, qh, ()));
                } else if interface == WlSeat::interface().name {
                    let version = min(version, WlSeat::interface().version);
                    state.seat = Some(registry.bind(name, version, qh, ()));
                } else if interface == ZwpPointerConstraintsV1::interface().name {
                    let version = min(version, ZwpPointerConstraintsV1::interface().version);
                    state.pointer_constraints = Some(registry.bind(name, version, qh, ()));
                } else if interface == XdgWmBase::interface().name {
                    let version = min(version, XdgWmBase::interface().version);
                    state.xdg_wm_base = Some(registry.bind(name, version, qh, ()));
                } else if interface == ZwlrLayerShellV1::interface().name {
                    let version = min(version, ZwlrLayerShellV1::interface().version);
                    state.layer_shell = Some(registry.bind(name, version, qh, ()));
                } else if interface == ExtForeignToplevelListV1::interface().name {
                    let version = min(version, ExtForeignToplevelListV1::interface().version);
                    state.ext_foreign_toplevel_list = Some(registry.bind(name, version, qh, ()));
                } else if interface == ZwlrForeignToplevelManagerV1::interface().name {
                    let version = min(version, ZwlrForeignToplevelManagerV1::interface().version);
                    state.foreign_toplevel_manager = Some(registry.bind(name, version, qh, ()));
                } else if interface == TahoeGlassManagerV1::interface().name {
                    let version = min(version, TahoeGlassManagerV1::interface().version);
                    state.tahoe_glass_manager = Some(registry.bind(name, version, qh, ()));
                } else if interface == WpSinglePixelBufferManagerV1::interface().name {
                    let version = min(version, WpSinglePixelBufferManagerV1::interface().version);
                    state.spbm = Some(registry.bind(name, version, qh, ()));
                } else if interface == WpViewporter::interface().name {
                    let version = min(version, WpViewporter::interface().version);
                    state.viewporter = Some(registry.bind(name, version, qh, ()));
                } else if interface == WlShm::interface().name {
                    let version = min(version, WlShm::interface().version);
                    state.shm = Some(registry.bind(name, version, qh, ()));
                } else if interface == WlOutput::interface().name {
                    let version = min(version, WlOutput::interface().version);
                    let output = registry.bind(name, version, qh, ());
                    state.outputs.insert(output, String::new());
                }

                let global = Global {
                    name,
                    interface,
                    version,
                };
                state.globals.push(global);
            }
            wl_registry::Event::GlobalRemove { .. } => (),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: <WlOutput as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_output::Event::Geometry { .. } => (),
            wl_output::Event::Mode { .. } => (),
            wl_output::Event::Done => (),
            wl_output::Event::Scale { .. } => (),
            wl_output::Event::Name { name } => {
                *state.outputs.get_mut(output).unwrap() = name;
            }
            wl_output::Event::Description { .. } => (),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<WlCompositor, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlCompositor,
        _event: <WlCompositor as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<WlSubcompositor, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlSubcompositor,
        event: <WlSubcompositor as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let _ = event;
    }
}

impl Dispatch<WlSubsurface, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlSubsurface,
        event: <WlSubsurface as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let _ = event;
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        event: <WlSeat as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let _ = event;
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlPointer,
        event: <WlPointer as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let _ = event;
    }
}

impl Dispatch<ZwpPointerConstraintsV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpPointerConstraintsV1,
        event: <ZwpPointerConstraintsV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let _ = event;
    }
}

impl Dispatch<ZwpLockedPointerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpLockedPointerV1,
        event: <ZwpLockedPointerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let _ = event;
    }
}

impl Dispatch<XdgWmBase, ()> for State {
    fn event(
        _state: &mut Self,
        xdg_wm_base: &XdgWmBase,
        event: <XdgWmBase as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            xdg_wm_base::Event::Ping { serial } => {
                xdg_wm_base.pong(serial);
            }
            _ => unreachable!(),
        }
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrLayerShellV1,
        _event: <ZwlrLayerShellV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ExtForeignToplevelListV1,
        event: <ExtForeignToplevelListV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } => {
                state.ext_foreign_toplevels.push(ExtForeignToplevelMeta {
                    handle: toplevel,
                    identifier: None,
                    title: None,
                    app_id: None,
                    done: false,
                    closed: false,
                });
            }
            ext_foreign_toplevel_list_v1::Event::Finished => (),
            _ => (),
        }
    }

    wayland_client::event_created_child!(State, ExtForeignToplevelListV1, [
        0 => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ExtForeignToplevelHandleV1,
        event: <ExtForeignToplevelHandleV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let Some(meta) = state
            .ext_foreign_toplevels
            .iter_mut()
            .find(|m| &m.handle == proxy)
        else {
            return;
        };
        match event {
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                meta.identifier = Some(identifier);
            }
            ext_foreign_toplevel_handle_v1::Event::Title { title } => {
                meta.title = Some(title);
            }
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                meta.app_id = Some(app_id);
            }
            ext_foreign_toplevel_handle_v1::Event::Done => {
                meta.done = true;
            }
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                meta.closed = true;
            }
            _ => (),
        }
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrForeignToplevelManagerV1,
        event: <ZwlrForeignToplevelManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } => {
                state.wlr_foreign_toplevel_meta.push(WlrForeignToplevelMeta {
                    handle: toplevel.clone(),
                    title: None,
                    app_id: None,
                    done: false,
                    closed: false,
                });
                state.foreign_toplevels.push(toplevel);
            }
            zwlr_foreign_toplevel_manager_v1::Event::Finished => (),
            _ => (),
        }
    }

    wayland_client::event_created_child!(State, ZwlrForeignToplevelManagerV1, [
        0 => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrForeignToplevelHandleV1,
        event: <ZwlrForeignToplevelHandleV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let Some(meta) = state
            .wlr_foreign_toplevel_meta
            .iter_mut()
            .find(|m| &m.handle == proxy)
        else {
            return;
        };
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => {
                meta.title = Some(title);
            }
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                meta.app_id = Some(app_id);
            }
            zwlr_foreign_toplevel_handle_v1::Event::Done => {
                meta.done = true;
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                meta.closed = true;
            }
            _ => (),
        }
    }
}

impl Dispatch<TahoeGlassManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &TahoeGlassManagerV1,
        _event: <TahoeGlassManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<TahoeGlassSurfaceV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &TahoeGlassSurfaceV1,
        _event: <TahoeGlassSurfaceV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<WlSurface, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlSurface,
        event: <WlSurface as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_surface::Event::Enter { .. } => (),
            wl_surface::Event::Leave { .. } => (),
            wl_surface::Event::PreferredBufferScale { .. } => (),
            wl_surface::Event::PreferredBufferTransform { .. } => (),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &XdgSurface,
        event: <XdgSurface as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            xdg_surface::Event::Configure { serial } => {
                if let Some(popup) = state
                    .popups
                    .iter_mut()
                    .find(|p| p.xdg_surface == *xdg_surface)
                {
                    popup.configure_serial = Some(serial);
                    return;
                }
                let window = state
                    .windows
                    .iter_mut()
                    .find(|w| w.xdg_surface == *xdg_surface)
                    .unwrap();
                let configure = window.pending_configure.clone();
                window.configures_received.push((serial, configure));
            }
            _ => unreachable!(),
        }
    }
}

impl Dispatch<XdgPopup, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &XdgPopup,
        event: <XdgPopup as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            xdg_popup::Event::Configure {
                x: _,
                y: _,
                width: _,
                height: _,
            } => (),
            _ => (),
        }
    }
}

impl Dispatch<XdgPositioner, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &XdgPositioner,
        event: <XdgPositioner as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let _ = event;
    }
}

impl Dispatch<XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        xdg_toplevel: &XdgToplevel,
        event: <XdgToplevel as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let window = state
            .windows
            .iter_mut()
            .find(|w| w.xdg_toplevel == *xdg_toplevel)
            .unwrap();

        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                let configure = &mut window.pending_configure;
                configure.size = (width, height);
                configure.states = states
                    .chunks_exact(4)
                    .flat_map(TryInto::<[u8; 4]>::try_into)
                    .map(u32::from_ne_bytes)
                    .flat_map(xdg_toplevel::State::try_from)
                    .collect();
            }
            xdg_toplevel::Event::Close => {
                window.close_requested = true;
            }
            xdg_toplevel::Event::ConfigureBounds { width, height } => {
                window.pending_configure.bounds = Some((width, height));
            }
            xdg_toplevel::Event::WmCapabilities { capabilities } => {
                window.wm_capabilities = capabilities
                    .chunks_exact(4)
                    .flat_map(TryInto::<[u8; 4]>::try_into)
                    .map(u32::from_ne_bytes)
                    .flat_map(xdg_toplevel::WmCapabilities::try_from)
                    .collect();
            }
            _ => unreachable!(),
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: <ZwlrLayerSurfaceV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let layer_surface = state
            .layers
            .iter_mut()
            .find(|w| w.layer_surface == *layer_surface)
            .unwrap();

        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                let configure = LayerConfigure {
                    size: (width, height),
                };
                layer_surface.configures_received.push((serial, configure));
            }
            zwlr_layer_surface_v1::Event::Closed => layer_surface.close_requested = true,
            _ => unreachable!(),
        }
    }
}

impl Dispatch<WlBuffer, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlBuffer,
        event: <WlBuffer as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_buffer::Event::Release => (),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<WlShm, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlShm,
        event: <WlShm as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_shm::Event::Format { .. } => (),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<WlShmPool, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlShmPool,
        event: <WlShmPool as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let _ = event;
    }
}

impl Dispatch<WpSinglePixelBufferManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WpSinglePixelBufferManagerV1,
        _event: <WpSinglePixelBufferManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<WpViewporter, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewporter,
        _event: <WpViewporter as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<WpViewport, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewport,
        _event: <WpViewport as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}
