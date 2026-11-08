#![cfg(feature = "wayland_frontend")]

use std::{
    os::{fd::AsFd, unix::net::UnixStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    reexports::wayland_server::{
        Client, Display,
        backend::ClientData,
        protocol::{wl_buffer::WlBuffer, wl_output::WlOutput, wl_surface::WlSurface},
    },
    utils::Serial,
    wayland::{
        buffer::BufferHandler,
        compositor::{CompositorClientState, CompositorHandler, CompositorState},
        shell::wlr_layer::{Layer, LayerSurface, WlrLayerShellHandler, WlrLayerShellState},
        shm::{ShmHandler, ShmState},
    },
};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, delegate_noop,
    protocol::{wl_buffer, wl_callback, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1 as shell, zwlr_layer_surface_v1 as layer,
};

#[derive(Debug, Default)]
struct ClientState(CompositorClientState);
impl ClientData for ClientState {}

struct State {
    compositor: CompositorState,
    shm: ShmState,
    shell: WlrLayerShellState,
    layers: Vec<LayerSurface>,
    commits: Vec<Option<Serial>>,
}
impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().0
    }
    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        if let Some(layer) = self.layers.iter().find(|layer| layer.wl_surface() == surface) {
            self.commits
                .push(layer.with_cached_state(|state| state.last_acked.map(|configure| configure.serial)));
        }
    }
}
impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _: &WlBuffer) {}
}
impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}
impl WlrLayerShellHandler for State {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.shell
    }
    fn new_layer_surface(&mut self, surface: LayerSurface, _: Option<WlOutput>, _: Layer, _: String) {
        self.layers.push(surface);
    }
}
smithay::delegate_dispatch2!(State);

#[derive(Default)]
struct Probe {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    shell: Option<shell::ZwlrLayerShellV1>,
    configures: Vec<u32>,
}
impl Dispatch<wl_registry::WlRegistry, ()> for Probe {
    fn event(
        probe: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        queue: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => probe.compositor = Some(registry.bind(name, version.min(5), queue, ())),
                "wl_shm" => probe.shm = Some(registry.bind(name, version.min(2), queue, ())),
                "zwlr_layer_shell_v1" => probe.shell = Some(registry.bind(name, version.min(5), queue, ())),
                _ => (),
            }
        }
    }
}
impl Dispatch<layer::ZwlrLayerSurfaceV1, ()> for Probe {
    fn event(
        probe: &mut Self,
        _: &layer::ZwlrLayerSurfaceV1,
        event: layer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let layer::Event::Configure { serial, .. } = event {
            probe.configures.push(serial);
        }
    }
}
impl Dispatch<wl_callback::WlCallback, Arc<AtomicBool>> for Probe {
    fn event(
        _: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        done: &Arc<AtomicBool>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        done.store(true, Ordering::Relaxed);
    }
}
delegate_noop!(Probe: wl_compositor::WlCompositor);
delegate_noop!(Probe: wl_surface::WlSurface);
delegate_noop!(Probe: ignore wl_shm::WlShm);
delegate_noop!(Probe: wl_shm_pool::WlShmPool);
delegate_noop!(Probe: ignore wl_buffer::WlBuffer);
delegate_noop!(Probe: shell::ZwlrLayerShellV1);

struct Runtime {
    display: Display<State>,
    state: State,
}
struct Wire {
    connection: Connection,
    queue: EventQueue<Probe>,
    probe: Probe,
}
impl Runtime {
    fn new() -> (Self, Wire) {
        let display = Display::<State>::new().unwrap();
        let mut runtime = Self {
            state: State {
                compositor: CompositorState::new::<State>(&display.handle()),
                shm: ShmState::new::<State>(&display.handle(), []),
                shell: WlrLayerShellState::new::<State>(&display.handle()),
                layers: Vec::new(),
                commits: Vec::new(),
            },
            display,
        };
        let (server, socket) = UnixStream::pair().unwrap();
        runtime
            .display
            .handle()
            .insert_client(server, Arc::new(ClientState::default()))
            .unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let queue = connection.new_event_queue();
        connection.display().get_registry(&queue.handle(), ());
        let mut wire = Wire {
            connection,
            queue,
            probe: Probe::default(),
        };
        wire.sync(&mut runtime).unwrap();
        (runtime, wire)
    }
    fn configure(&mut self, size: (i32, i32)) -> u32 {
        let layer = &self.state.layers[0];
        layer.with_pending_state(|state| state.size = Some(size.into()));
        layer.send_configure().into()
    }
}
impl Wire {
    fn sync(&mut self, runtime: &mut Runtime) -> Result<(), Box<dyn std::error::Error>> {
        let done = Arc::new(AtomicBool::new(false));
        self.connection.display().sync(&self.queue.handle(), done.clone());
        let deadline = Instant::now() + Duration::from_secs(2);
        while !done.load(Ordering::Relaxed) {
            self.connection.flush()?;
            runtime.display.dispatch_clients(&mut runtime.state)?;
            runtime.display.flush_clients()?;
            self.queue.dispatch_pending(&mut self.probe)?;
            if done.load(Ordering::Relaxed) {
                break;
            }
            let Some(read) = self.queue.prepare_read() else {
                continue;
            };
            let fd = read.connection_fd();
            let timeout = Timespec::try_from(
                deadline
                    .checked_duration_since(Instant::now())
                    .expect("roundtrip timed out"),
            )
            .unwrap();
            assert_ne!(poll(&mut [PollFd::new(&fd, PollFlags::IN)], Some(&timeout))?, 0);
            read.read()?;
            self.queue.dispatch_pending(&mut self.probe)?;
        }
        Ok(())
    }
    fn layer(&self) -> (wl_surface::WlSurface, layer::ZwlrLayerSurfaceV1) {
        let queue = self.queue.handle();
        let surface = self.probe.compositor.as_ref().unwrap().create_surface(&queue, ());
        let layer = self.probe.shell.as_ref().unwrap().get_layer_surface(
            &surface,
            None,
            shell::Layer::Top,
            "test".into(),
            &queue,
            (),
        );
        layer.set_size(20, 10);
        (surface, layer)
    }
    fn buffer(&self) -> wl_buffer::WlBuffer {
        let file = tempfile::tempfile().unwrap();
        file.set_len(20 * 10 * 4).unwrap();
        let pool =
            self.probe
                .shm
                .as_ref()
                .unwrap()
                .create_pool(file.as_fd(), 20 * 10 * 4, &self.queue.handle(), ());
        let buffer = pool.create_buffer(
            0,
            20,
            10,
            20 * 4,
            wl_shm::Format::Argb8888,
            &self.queue.handle(),
            (),
        );
        pool.destroy();
        buffer
    }
}

struct Unmapped {
    runtime: Runtime,
    wire: Wire,
    surface: wl_surface::WlSurface,
    layer: layer::ZwlrLayerSurfaceV1,
    buffer: wl_buffer::WlBuffer,
    old: u32,
}
impl Unmapped {
    fn new() -> Self {
        let (mut runtime, mut wire) = Runtime::new();
        let (surface, layer) = wire.layer();
        surface.commit();
        wire.sync(&mut runtime).unwrap();
        let initial = runtime.configure((20, 10));
        wire.sync(&mut runtime).unwrap();
        let buffer = wire.buffer();
        layer.ack_configure(initial);
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        wire.sync(&mut runtime).unwrap();
        assert_eq!(runtime.state.commits.last(), Some(&Some(initial.into())));
        let old = runtime.configure((30, 12));
        wire.sync(&mut runtime).unwrap();
        surface.attach(None, 0, 0);
        surface.commit();
        wire.sync(&mut runtime).unwrap();
        assert_eq!(runtime.state.commits.last(), Some(&None));
        layer.set_size(20, 10);
        Self {
            runtime,
            wire,
            surface,
            layer,
            buffer,
            old,
        }
    }
}

#[test]
fn an_old_configure_cannot_map_a_layer_before_its_new_initial_configure() {
    stale_buffer_is_rejected(false);
}

#[test]
fn an_old_configure_cannot_map_a_layer_after_its_new_initial_configure() {
    stale_buffer_is_rejected(true);
}

fn stale_buffer_is_rejected(send_new_initial: bool) {
    let mut case = Unmapped::new();
    if send_new_initial {
        case.surface.commit();
        case.wire.sync(&mut case.runtime).unwrap();
        case.runtime.configure((20, 10));
        case.wire.sync(&mut case.runtime).unwrap();
    }
    case.layer.ack_configure(case.old);
    case.surface.attach(Some(&case.buffer), 0, 0);
    case.surface.commit();
    assert!(
        case.wire.sync(&mut case.runtime).is_err(),
        "a configure from the previous mapping authorized a new buffer"
    );
    let error = case.wire.connection.protocol_error().unwrap();
    assert_eq!(error.object_interface, "zwlr_layer_surface_v1");
    assert_eq!(error.code, layer::Error::InvalidSurfaceState as u32);
}

#[test]
fn an_in_flight_ack_is_accepted_without_a_buffer_then_the_new_configure_can_map() {
    let mut case = Unmapped::new();
    case.layer.ack_configure(case.old);
    case.surface.commit();
    case.wire.sync(&mut case.runtime).unwrap();
    assert_eq!(case.runtime.state.commits.last(), Some(&None));
    let new = case.runtime.configure((20, 10));
    case.wire.sync(&mut case.runtime).unwrap();
    assert_eq!(case.wire.probe.configures.last(), Some(&new));
    case.layer.ack_configure(new);
    case.surface.attach(Some(&case.buffer), 0, 0);
    case.surface.commit();
    case.wire.sync(&mut case.runtime).unwrap();
    assert_eq!(case.runtime.state.commits.last(), Some(&Some(new.into())));
}
