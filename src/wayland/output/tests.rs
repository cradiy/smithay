use super::*;
use crate::{
    output::{PhysicalProperties, Subpixel},
    utils::Transform,
    wayland::compositor::{CompositorClientState, CompositorHandler, CompositorState},
};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::{
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, delegate_noop,
    protocol::{wl_callback, wl_output, wl_registry},
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1 as manager, zxdg_output_v1 as xdg,
};
use wayland_server::{Client, Display, backend::ClientData};

#[derive(Debug, Default)]
struct ClientState(CompositorClientState);
impl ClientData for ClientState {}
struct State {
    compositor: CompositorState,
}
impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().0
    }
    fn commit(&mut self, _: &wayland_server::protocol::wl_surface::WlSurface) {}
}
impl OutputHandler for State {}
crate::delegate_dispatch2!(State);

#[derive(Default)]
struct Probe {
    name: Option<u32>,
    manager: Option<manager::ZxdgOutputManagerV1>,
    metadata: usize,
    removed: bool,
}
impl Dispatch<wl_registry::WlRegistry, ()> for Probe {
    fn event(
        probe: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global { name, interface, .. } => match interface.as_str() {
                "wl_output" => probe.name = Some(name),
                "zxdg_output_manager_v1" => probe.manager = Some(registry.bind(name, 3, qh, ())),
                _ => (),
            },
            wl_registry::Event::GlobalRemove { name } if probe.name == Some(name) => probe.removed = true,
            _ => (),
        }
    }
}
impl Dispatch<wl_output::WlOutput, ()> for Probe {
    fn event(
        probe: &mut Self,
        _: &wl_output::WlOutput,
        _: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        probe.metadata += 1;
    }
}
impl Dispatch<xdg::ZxdgOutputV1, ()> for Probe {
    fn event(
        probe: &mut Self,
        _: &xdg::ZxdgOutputV1,
        _: xdg::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        probe.metadata += 1;
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
delegate_noop!(Probe: manager::ZxdgOutputManagerV1);
fn sync(
    display: &mut Display<State>,
    state: &mut State,
    connection: &Connection,
    queue: &mut EventQueue<Probe>,
    probe: &mut Probe,
) {
    let done = Arc::new(AtomicBool::new(false));
    connection.display().sync(&queue.handle(), done.clone());
    let deadline = Instant::now() + Duration::from_secs(2);
    while !done.load(Ordering::Relaxed) {
        connection.flush().unwrap();
        display.dispatch_clients(state).unwrap();
        display.flush_clients().unwrap();
        queue.dispatch_pending(probe).unwrap();
        if done.load(Ordering::Relaxed) {
            break;
        }
        if let Some(read) = queue.prepare_read() {
            let fd = read.connection_fd();
            let mut fds = [PollFd::new(&fd, PollFlags::IN)];
            let timeout = Timespec::try_from(
                deadline
                    .checked_duration_since(Instant::now())
                    .expect("roundtrip timed out"),
            )
            .unwrap();
            assert_ne!(poll(&mut fds, Some(&timeout)).unwrap(), 0);
            read.read().unwrap();
        }
    }
}

#[test]
fn disabled_globals_accept_in_flight_binds_without_retaining_the_output() {
    let mut display = Display::<State>::new().unwrap();
    let mut state = State {
        compositor: CompositorState::new::<State>(&display.handle()),
    };
    let _manager = OutputManagerState::new_with_xdg_output::<State>(&display.handle());
    let output = Output::new(
        "test-output".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Test".into(),
            model: "Virtual".into(),
            serial_number: String::new(),
        },
    );
    let mode = Mode {
        size: (8, 4).into(),
        refresh: 60_000,
    };
    output.set_preferred(mode);
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        Some(Scale::Integer(1)),
        Some((0, 0).into()),
    );
    let weak = output.downgrade();
    let global = output.create_global_with_handle::<State>(&display.handle());
    drop(output);
    assert!(weak.upgrade().is_some());
    let (server, socket) = UnixStream::pair().unwrap();
    display
        .handle()
        .insert_client(server, Arc::new(ClientState::default()))
        .unwrap();
    let connection = Connection::from_socket(socket).unwrap();
    let mut queue = connection.new_event_queue();
    let mut probe = Probe::default();
    let registry = connection.display().get_registry(&queue.handle(), ());
    sync(&mut display, &mut state, &connection, &mut queue, &mut probe);
    let bound: wl_output::WlOutput = registry.bind(probe.name.unwrap(), 4, &queue.handle(), ());
    sync(&mut display, &mut state, &connection, &mut queue, &mut probe);
    assert!(probe.metadata > 0);
    let previous = probe.metadata;
    let in_flight = weak.upgrade().unwrap();
    global.disable::<State>(&display.handle());
    let raced: wl_output::WlOutput = registry.bind(probe.name.unwrap(), 4, &queue.handle(), ());
    let manager = probe.manager.as_ref().unwrap();
    let old_xdg = manager.get_xdg_output(&bound, &queue.handle(), ());
    let raced_xdg = manager.get_xdg_output(&raced, &queue.handle(), ());
    sync(&mut display, &mut state, &connection, &mut queue, &mut probe);
    assert!(probe.removed);
    assert_eq!(
        probe.metadata, previous,
        "retired output and xdg-output objects must stay inert"
    );
    drop(in_flight);
    assert!(
        weak.upgrade().is_none(),
        "the registration and bound resources must release the retired output"
    );
    display.handle().remove_global::<State>(global.global());
    old_xdg.destroy();
    raced_xdg.destroy();
    bound.release();
    raced.release();
    sync(&mut display, &mut state, &connection, &mut queue, &mut probe);
}
