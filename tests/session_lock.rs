#![cfg(feature = "wayland_frontend")]

use std::{
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use smithay::{
    output::{Output, PhysicalProperties, Subpixel},
    reexports::wayland_server::{
        Client, Display,
        backend::{ClientData, GlobalId},
        protocol::{wl_output::WlOutput, wl_surface::WlSurface},
    },
    wayland::{
        compositor::{CompositorClientState, CompositorHandler, CompositorState},
        output::OutputHandler,
        session_lock::{LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker},
    },
};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, delegate_noop,
    protocol::{wl_callback, wl_compositor, wl_output, wl_registry, wl_surface},
};
use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1 as manager, ext_session_lock_surface_v1 as surface,
    ext_session_lock_v1 as lock,
};

#[derive(Debug, Default)]
struct ClientState(CompositorClientState);
impl ClientData for ClientState {}
struct State {
    compositor: CompositorState,
    locks: SessionLockManagerState,
    pending: Option<SessionLocker>,
    surfaces: Vec<LockSurface>,
}
impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().0
    }
    fn commit(&mut self, _: &WlSurface) {}
}
impl OutputHandler for State {}
impl SessionLockHandler for State {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.locks
    }
    fn lock(&mut self, confirmation: SessionLocker) {
        self.pending = Some(confirmation);
    }
    fn unlock(&mut self) {
        panic!("an unconfirmed lock cannot unlock");
    }
    fn new_surface(&mut self, surface: LockSurface, _: WlOutput) {
        surface.with_pending_state(|state| state.size = Some((16, 12).into()));
        self.surfaces.push(surface);
    }
}
smithay::delegate_dispatch2!(State);

#[derive(Default)]
struct Probe {
    compositor: Option<wl_compositor::WlCompositor>,
    manager: Option<manager::ExtSessionLockManagerV1>,
    outputs: Vec<Vec<wl_output::WlOutput>>,
    configures: Vec<(u32, u32)>,
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
                "wl_output" => probe.outputs.push(
                    (0..2)
                        .map(|_| registry.bind(name, version.min(4), queue, ()))
                        .collect(),
                ),
                "ext_session_lock_manager_v1" => probe.manager = Some(registry.bind(name, 1, queue, ())),
                _ => (),
            }
        }
    }
}
impl Dispatch<surface::ExtSessionLockSurfaceV1, ()> for Probe {
    fn event(
        probe: &mut Self,
        _: &surface::ExtSessionLockSurfaceV1,
        event: surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let surface::Event::Configure { width, height, .. } = event {
            probe.configures.push((width, height));
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
delegate_noop!(Probe: ignore wl_output::WlOutput);
delegate_noop!(Probe: manager::ExtSessionLockManagerV1);
delegate_noop!(Probe: ignore lock::ExtSessionLockV1);

struct Runtime {
    display: Display<State>,
    state: State,
    outputs: Vec<(Output, GlobalId)>,
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
                locks: SessionLockManagerState::new::<State, _>(&display.handle(), |_| true),
                pending: None,
                surfaces: Vec::new(),
            },
            display,
            outputs: Vec::new(),
        };
        runtime.add_output("first");
        runtime.add_output("second");
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
    fn add_output(&mut self, name: &str) {
        let output = Output::new(
            name.into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Test".into(),
                model: "Test".into(),
                serial_number: String::new(),
            },
        );
        let global = output.create_global::<State>(&self.display.handle());
        self.outputs.push((output, global));
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
            let mut fds = [PollFd::new(&fd, PollFlags::IN)];
            let timeout = Timespec::try_from(
                deadline
                    .checked_duration_since(Instant::now())
                    .expect("roundtrip timed out"),
            )
            .unwrap();
            assert_ne!(poll(&mut fds, Some(&timeout))?, 0);
            read.read()?;
            self.queue.dispatch_pending(&mut self.probe)?;
        }
        Ok(())
    }
    fn lock(&self) -> lock::ExtSessionLockV1 {
        self.probe
            .manager
            .as_ref()
            .unwrap()
            .lock(&self.queue.handle(), ())
    }
    fn surface(
        &self,
        lock: &lock::ExtSessionLockV1,
        output: usize,
        binding: usize,
    ) -> surface::ExtSessionLockSurfaceV1 {
        let surface = self
            .probe
            .compositor
            .as_ref()
            .unwrap()
            .create_surface(&self.queue.handle(), ());
        lock.get_lock_surface(
            &surface,
            &self.probe.outputs[output][binding],
            &self.queue.handle(),
            (),
        )
    }
}

#[test]
fn rebinding_an_output_cannot_bypass_duplicate_lock_surface_validation() {
    for release_binding in [false, true] {
        let (mut runtime, mut wire) = Runtime::new();
        let lock = wire.lock();
        let _first = wire.surface(&lock, 0, 0);
        wire.sync(&mut runtime).unwrap();
        assert_eq!(wire.probe.configures, [(16, 12)]);
        if release_binding {
            wire.probe.outputs[0][0].release();
        }
        let _duplicate = wire.surface(&lock, 0, 1);
        assert!(
            wire.sync(&mut runtime).is_err(),
            "rebinding wl_output bypassed duplicate_output"
        );
        let error = wire.connection.protocol_error().unwrap();
        assert_eq!(error.object_interface, "ext_session_lock_v1");
        assert_eq!(error.code, lock::Error::DuplicateOutput as u32);
        assert_eq!(runtime.state.surfaces.len(), 1);
    }
}

#[test]
fn distinct_and_replacement_outputs_keep_independent_lock_surfaces_without_retaining_retired_outputs() {
    let (mut runtime, mut wire) = Runtime::new();
    let lock = wire.lock();
    let _first = wire.surface(&lock, 0, 0);
    let _second = wire.surface(&lock, 1, 1);
    wire.sync(&mut runtime).unwrap();
    assert_eq!(runtime.state.surfaces.len(), 2);
    let (retired, global) = runtime.outputs.remove(0);
    let weak = retired.downgrade();
    runtime.display.handle().remove_global::<State>(global);
    drop(retired);
    assert!(
        weak.upgrade().is_none(),
        "lock bookkeeping retained a retired output"
    );
    runtime.add_output("first");
    wire.sync(&mut runtime).unwrap();
    let _replacement = wire.surface(&lock, 2, 0);
    wire.sync(&mut runtime).unwrap();
    assert_eq!(runtime.state.surfaces.len(), 3);
    assert_eq!(wire.probe.configures, [(16, 12); 3]);
}
