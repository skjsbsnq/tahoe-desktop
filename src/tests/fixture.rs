use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::time::Duration;

use calloop::generic::Generic;
use calloop::{EventLoop, Interest, LoopHandle, Mode, PostAction};
use niri_config::Config;
use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer;
use smithay::utils::Transform;
use wayland_client::protocol::wl_surface::WlSurface;

use super::client::{Client, ClientId, LayerConfigureProps};
use super::server::Server;
use crate::niri::{NewClient, Niri};

pub struct Fixture {
    pub event_loop: EventLoop<'static, State>,
    pub handle: LoopHandle<'static, State>,
    pub state: State,
}

pub struct State {
    pub server: Server,
    pub clients: Vec<Client>,
}

impl Fixture {
    pub fn new() -> Self {
        Self::with_config(Config::default())
    }

    pub fn with_config(config: Config) -> Self {
        let event_loop = EventLoop::try_new().unwrap();
        let handle = event_loop.handle();

        let server = Server::new(config);
        let fd = server.event_loop.as_fd().try_clone_to_owned().unwrap();
        let source = Generic::new(fd, Interest::READ, Mode::Level);
        handle
            .insert_source(source, |_, _, state: &mut State| {
                state.server.dispatch();
                Ok(PostAction::Continue)
            })
            .unwrap();

        let state = State {
            server,
            clients: Vec::new(),
        };

        Self {
            event_loop,
            handle,
            state,
        }
    }

    pub fn dispatch(&mut self) {
        self.event_loop
            .dispatch(Duration::ZERO, &mut self.state)
            .unwrap();
    }

    pub fn niri_state(&mut self) -> &mut crate::niri::State {
        &mut self.state.server.state
    }

    pub fn niri(&mut self) -> &mut Niri {
        &mut self.niri_state().niri
    }

    pub fn niri_output(&self, n: u8) -> Output {
        let niri = &self.state.server.state.niri;
        let idx = usize::from(n - 1);
        let output = niri.global_space.outputs().nth(idx).unwrap();
        output.clone()
    }

    pub fn niri_focus_output(&mut self, n: u8) {
        let niri = &mut self.state.server.state.niri;
        let idx = usize::from(n - 1);
        let output = niri.global_space.outputs().nth(idx).unwrap();
        niri.layout.focus_output(output);
    }

    pub fn niri_complete_animations(&mut self) {
        let niri = self.niri();
        niri.clock.set_complete_instantly(true);
        niri.advance_animations();
        niri.clock.set_complete_instantly(false);
    }

    pub fn add_output(&mut self, n: u8, size: (u16, u16)) {
        self.add_output_with_scale_transform(n, size, 1., Transform::Normal);
    }

    /// Add a headless output, then apply scale/transform through the real Output state path.
    ///
    /// `Niri::add_output` overwrites scale/transform from config or guessed scale, so fixtures that
    /// need a specific scale/transform re-apply them and refresh layout size afterward.
    pub fn add_output_with_scale_transform(
        &mut self,
        n: u8,
        size: (u16, u16),
        scale: f64,
        transform: Transform,
    ) {
        {
            let state = self.niri_state();
            let niri = &mut state.niri;
            state
                .backend
                .headless()
                .add_output_with_scale_transform(niri, n, size, scale, transform);
        }

        let output = self.niri_output(n);
        output.change_current_state(
            None,
            Some(transform),
            Some(smithay::output::Scale::Fractional(scale)),
            None,
        );
        self.niri().layout.update_output_size(&output);
    }

    /// Map a layer surface through the real layer-shell configure/ack/commit sequence.
    pub fn map_layer(
        &mut self,
        id: ClientId,
        output: Option<u8>,
        layer_kind: Layer,
        namespace: &str,
        props: LayerConfigureProps,
        buffer_size: (u16, u16),
    ) -> WlSurface {
        let output = output.map(|n| self.client(id).output(&format!("headless-{n}")));
        let surface = {
            let layer = self
                .client(id)
                .create_layer(output.as_ref(), layer_kind, namespace);
            let surface = layer.surface.clone();
            layer.set_configure_props(props);
            layer.commit();
            surface
        };
        self.roundtrip(id);

        let layer = self.client(id).layer(&surface);
        layer.attach_new_buffer();
        layer.set_size(buffer_size.0, buffer_size.1);
        layer.ack_last_and_commit();
        self.double_roundtrip(id);
        surface
    }

    /// Unmap a layer through a null-buffer commit, preserving the production lifecycle path.
    pub fn unmap_layer(&mut self, id: ClientId, surface: &WlSurface) {
        let layer = self.client(id).layer(surface);
        layer.attach_null();
        layer.commit();
        self.double_roundtrip(id);
    }

    /// Remap a layer surface after [`Self::unmap_layer`] using its real configure/ack/commit path.
    pub fn remap_layer(
        &mut self,
        id: ClientId,
        surface: &WlSurface,
        props: LayerConfigureProps,
        buffer_size: (u16, u16),
    ) {
        let layer = self.client(id).layer(surface);
        layer.set_configure_props(props);
        layer.commit();
        self.double_roundtrip(id);

        let layer = self.client(id).layer(surface);
        layer.attach_new_buffer();
        layer.set_size(buffer_size.0, buffer_size.1);
        layer.ack_last_and_commit();
        self.double_roundtrip(id);
    }

    pub fn add_client(&mut self) -> ClientId {
        let (sock1, sock2) = UnixStream::pair().unwrap();
        self.niri().insert_client(NewClient {
            client: sock1,
            restricted: false,
            credentials_unknown: false,
        });

        let client = Client::new(sock2);
        let id = client.id;

        let fd = client.event_loop.as_fd().try_clone_to_owned().unwrap();
        let source = Generic::new(fd, Interest::READ, Mode::Level);
        self.handle
            .insert_source(source, move |_, _, state: &mut State| {
                state.client(id).dispatch();
                Ok(PostAction::Continue)
            })
            .unwrap();

        self.state.clients.push(client);
        self.roundtrip(id);
        id
    }

    pub fn client(&mut self, id: ClientId) -> &mut Client {
        self.state.client(id)
    }

    /// Drop a test client without a clean protocol teardown.
    ///
    /// Closes the client-side socket so the compositor sees an abnormal
    /// disconnect and destroys remaining client objects (including any
    /// still-alive `tahoe_glass_surface_v1`). Used by Tahoe glass lifecycle
    /// coverage; not a general production API.
    pub fn disconnect_client(&mut self, id: ClientId) {
        let pos = self
            .state
            .clients
            .iter()
            .position(|c| c.id == id)
            .expect("disconnect_client: unknown ClientId");
        let client = self.state.clients.remove(pos);
        drop(client);
        // Drain compositor cleanup for the closed socket.
        for _ in 0..8 {
            self.dispatch();
        }
    }

    pub fn roundtrip(&mut self, id: ClientId) {
        let client = self.state.client(id);
        let data = client.send_sync();
        while !data.done.load(Ordering::Relaxed) {
            self.dispatch();
        }
    }

    /// Roundtrip twice in a row.
    ///
    /// For some reason, when running tests on many threads at once, a single roundtrip is
    /// sometimes not sufficient to get the configure events to the client.
    ///
    /// I suspect that this is because these configure events are sent from the niri loop callback,
    /// so they arrive after the sync done event and don't get processed in that client dispatch
    /// cycle. I'm not sure why this would be dependent on multithreading. But if this is indeed
    /// the issue, then a double roundtrip fixes it.
    pub fn double_roundtrip(&mut self, id: ClientId) {
        self.roundtrip(id);
        self.roundtrip(id);
    }
}

impl State {
    pub fn client(&mut self, id: ClientId) -> &mut Client {
        self.clients.iter_mut().find(|c| c.id == id).unwrap()
    }
}
