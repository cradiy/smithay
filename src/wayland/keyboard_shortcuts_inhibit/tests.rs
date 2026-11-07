use std::{
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, delegate_noop,
    protocol::{wl_callback, wl_compositor, wl_registry, wl_seat, wl_surface},
};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1 as manager, zwp_keyboard_shortcuts_inhibitor_v1 as inhibitor,
};
use wayland_server::{Client, Display, backend::ClientData};

use super::*;
use crate::{
    input::{SeatHandler, SeatState},
    wayland::{
        compositor::{CompositorClientState, CompositorHandler, CompositorState},
        pointer_constraints::PointerConstraintsHandler,
    },
};

#[derive(Debug, Default)]
struct ClientState(CompositorClientState);
impl ClientData for ClientState {}
struct State {
    compositor: CompositorState,
    seats: SeatState<Self>,
    shortcuts: KeyboardShortcutsInhibitState,
}
impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;
    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seats
    }
}
impl PointerConstraintsHandler for State {}
impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().0
    }
    fn commit(&mut self, _: &WlSurface) {}
}
impl KeyboardShortcutsInhibitHandler for State {
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut KeyboardShortcutsInhibitState {
        &mut self.shortcuts
    }
    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        inhibitor.activate();
    }
}
crate::delegate_dispatch2!(State);

#[derive(Default)]
struct Probe {
    compositor: Option<wl_compositor::WlCompositor>,
    manager: Option<manager::ZwpKeyboardShortcutsInhibitManagerV1>,
    seats: Vec<wl_seat::WlSeat>,
    active: usize,
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
                "wl_seat" => {
                    for _ in 0..2 {
                        probe.seats.push(registry.bind(name, 9, queue, ()));
                    }
                }
                "zwp_keyboard_shortcuts_inhibit_manager_v1" => {
                    probe.manager = Some(registry.bind(name, 1, queue, ()))
                }
                _ => (),
            }
        }
    }
}
impl Dispatch<inhibitor::ZwpKeyboardShortcutsInhibitorV1, ()> for Probe {
    fn event(
        probe: &mut Self,
        _: &inhibitor::ZwpKeyboardShortcutsInhibitorV1,
        event: inhibitor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let inhibitor::Event::Active = event {
            probe.active += 1;
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
delegate_noop!(Probe: ignore wl_seat::WlSeat);
delegate_noop!(Probe: manager::ZwpKeyboardShortcutsInhibitManagerV1);

struct Runtime {
    display: Display<State>,
    state: State,
    seat: Option<Seat<State>>,
}
struct Wire {
    connection: Connection,
    queue: EventQueue<Probe>,
    probe: Probe,
}
impl Runtime {
    fn new() -> (Self, Wire) {
        let display = Display::<State>::new().unwrap();
        let compositor = CompositorState::new::<State>(&display.handle());
        let shortcuts = KeyboardShortcutsInhibitState::new::<State>(&display.handle());
        let mut seats = SeatState::new();
        let seat = seats.new_wl_seat(&display.handle(), "test-seat");
        let (server, socket) = UnixStream::pair().unwrap();
        display
            .handle()
            .insert_client(server, Arc::new(ClientState::default()))
            .unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let queue = connection.new_event_queue();
        connection.display().get_registry(&queue.handle(), ());
        let mut runtime = Self {
            display,
            state: State {
                compositor,
                seats,
                shortcuts,
            },
            seat: Some(seat),
        };
        let mut wire = Wire {
            connection,
            queue,
            probe: Probe::default(),
        };
        wire.sync(&mut runtime);
        (runtime, wire)
    }
    fn remove_seat(&mut self) {
        let seat = self.seat.take().unwrap();
        self.display
            .handle()
            .remove_global::<State>(seat.global().unwrap());
        self.state.seats = SeatState::new();
    }
}
impl Wire {
    fn sync(&mut self, runtime: &mut Runtime) {
        let done = Arc::new(AtomicBool::new(false));
        self.connection.display().sync(&self.queue.handle(), done.clone());
        let deadline = Instant::now() + Duration::from_secs(2);
        while !done.load(Ordering::Relaxed) {
            self.connection.flush().unwrap();
            runtime.display.dispatch_clients(&mut runtime.state).unwrap();
            runtime.display.flush_clients().unwrap();
            self.queue.dispatch_pending(&mut self.probe).unwrap();
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
            assert_ne!(poll(&mut fds, Some(&timeout)).unwrap(), 0);
            read.read().unwrap();
            self.queue.dispatch_pending(&mut self.probe).unwrap();
        }
    }
    fn surface(&self) -> wl_surface::WlSurface {
        self.probe
            .compositor
            .as_ref()
            .unwrap()
            .create_surface(&self.queue.handle(), ())
    }
    fn inhibit(
        &self,
        surface: &wl_surface::WlSurface,
        seat: usize,
    ) -> inhibitor::ZwpKeyboardShortcutsInhibitorV1 {
        self.probe.manager.as_ref().unwrap().inhibit_shortcuts(
            surface,
            &self.probe.seats[seat],
            &self.queue.handle(),
            (),
        )
    }
}

#[test]
fn duplicate_inhibitors_are_rejected_across_bindings_of_the_same_logical_seat() {
    let (mut runtime, mut wire) = Runtime::new();
    let surface = wire.surface();
    let _first = wire.inhibit(&surface, 0);
    wire.sync(&mut runtime);
    let _duplicate = wire.inhibit(&surface, 1);
    wire.connection.flush().unwrap();
    runtime.display.dispatch_clients(&mut runtime.state).unwrap();
    runtime.display.flush_clients().unwrap();
    let result = wire.queue.blocking_dispatch(&mut wire.probe);
    assert!(
        result.is_err(),
        "rebinding wl_seat must not bypass already_inhibited"
    );
    let error = wire.connection.protocol_error().unwrap();
    assert_eq!(
        error.object_interface,
        "zwp_keyboard_shortcuts_inhibit_manager_v1"
    );
    assert_eq!(error.code, manager::Error::AlreadyInhibited as u32);
}

#[test]
fn retired_seat_releases_its_shared_index_and_leaves_existing_resources_inert() {
    let (mut runtime, mut wire) = Runtime::new();
    let surface = wire.surface();
    let second_surface = wire.surface();
    let first = wire.inhibit(&surface, 0);
    let second = wire.inhibit(&second_surface, 1);
    wire.sync(&mut runtime);
    let weak = Rc::downgrade(&SeatData::get(runtime.seat.as_ref().unwrap()).borrow().inhibitors);
    first.destroy();
    wire.sync(&mut runtime);
    second.destroy();
    wire.sync(&mut runtime);
    assert!(
        runtime.state.shortcuts.inhibitors.is_empty(),
        "destroyed protocol objects must retire every resource index entry"
    );
    runtime.remove_seat();
    assert!(
        weak.upgrade().is_none(),
        "empty resource aliases must not retain the retired seat's inhibitor index"
    );
    let active = wire.probe.active;
    let inert = wire.inhibit(&surface, 0);
    wire.sync(&mut runtime);
    assert_eq!(
        wire.probe.active, active,
        "a removed logical seat cannot grant inhibition"
    );
    inert.destroy();
    wire.sync(&mut runtime);
}
