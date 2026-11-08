#![cfg(feature = "wayland_frontend")]

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use smithay::{
    backend::input::{InputTime, TouchSlot},
    input::{
        SeatHandler, SeatState,
        touch::{DownEvent, TouchHandle, UpEvent},
    },
    reexports::wayland_server::{Client, Display, backend::ClientData, protocol::wl_surface::WlSurface},
    wayland::{
        compositor::{CompositorClientState, CompositorHandler, CompositorState},
        pointer_constraints::PointerConstraintsHandler,
    },
};
use std::{
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop,
    protocol::{wl_callback, wl_compositor, wl_registry, wl_seat, wl_surface, wl_touch},
};

#[derive(Debug, Default)]
struct ClientState(CompositorClientState);
impl ClientData for ClientState {}
struct State {
    compositor: CompositorState,
    seats: SeatState<Self>,
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
smithay::delegate_dispatch2!(State);

#[derive(Default)]
struct Probe {
    compositor: Option<wl_compositor::WlCompositor>,
    seat: Option<wl_seat::WlSeat>,
    events: Vec<(usize, Event)>,
    coordinates: Vec<(usize, i32, f64, f64)>,
}
#[derive(Debug, PartialEq)]
enum Event {
    Down(i32),
    Up(i32),
    Frame,
    Cancel,
}
impl Dispatch<wl_registry::WlRegistry, ()> for Probe {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, .. } = event {
            match interface.as_str() {
                "wl_compositor" => state.compositor = Some(registry.bind(name, 5, qh, ())),
                "wl_seat" => state.seat = Some(registry.bind(name, 9, qh, ())),
                _ => (),
            }
        }
    }
}
impl Dispatch<wl_touch::WlTouch, usize> for Probe {
    fn event(
        state: &mut Self,
        _: &wl_touch::WlTouch,
        event: wl_touch::Event,
        id: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let event = match event {
            wl_touch::Event::Down { id: slot, x, y, .. } => {
                state.coordinates.push((*id, slot, x, y));
                Event::Down(slot)
            }
            wl_touch::Event::Motion { id: slot, x, y, .. } => {
                state.coordinates.push((*id, slot, x, y));
                return;
            }
            wl_touch::Event::Up { id, .. } => Event::Up(id),
            wl_touch::Event::Frame => Event::Frame,
            wl_touch::Event::Cancel => Event::Cancel,
            _ => panic!("unexpected touch event {event:?}"),
        };
        state.events.push((*id, event));
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

struct Runtime {
    display: Display<State>,
    state: State,
    touch: TouchHandle<State>,
}
struct Wire {
    connection: Connection,
    queue: EventQueue<Probe>,
    probe: Probe,
    client: Client,
}
impl Runtime {
    fn new() -> Self {
        let display = Display::<State>::new().unwrap();
        let compositor = CompositorState::new::<State>(&display.handle());
        let mut seats = SeatState::new();
        let mut seat = seats.new_wl_seat(&display.handle(), "test-seat");
        let touch = seat.add_touch();
        Self {
            display,
            state: State { compositor, seats },
            touch,
        }
    }
    fn client(&mut self) -> Wire {
        let (server, socket) = UnixStream::pair().unwrap();
        let client = self
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
            client,
        };
        wire.sync(self);
        wire
    }
    fn down(&mut self, surface: &WlSurface, id: u32) {
        self.touch.clone().down(
            &mut self.state,
            Some((surface.clone(), (0., 0.).into())),
            &DownEvent {
                slot: TouchSlot::from(Some(id)),
                location: (1., 1.).into(),
                serial: (100 + id).into(),
                time: InputTime::from_micros(1000),
            },
        );
    }
    fn frame(&mut self) {
        self.touch.clone().frame(&mut self.state);
    }
    fn cancel(&mut self) {
        self.touch.clone().cancel(&mut self.state);
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
            let timeout = Timespec::try_from(
                deadline
                    .checked_duration_since(Instant::now())
                    .expect("roundtrip timed out"),
            )
            .unwrap();
            assert_ne!(
                poll(&mut [PollFd::new(&fd, PollFlags::IN)], Some(&timeout)).unwrap(),
                0
            );
            read.read().unwrap();
        }
    }
    fn surface(&mut self, runtime: &mut Runtime) -> WlSurface {
        let surface = self
            .probe
            .compositor
            .as_ref()
            .unwrap()
            .create_surface(&self.queue.handle(), ());
        self.sync(runtime);
        self.client
            .object_from_protocol_id::<WlSurface>(&runtime.display.handle(), surface.id().protocol_id())
            .unwrap()
    }
    fn touch(&self, id: usize) -> wl_touch::WlTouch {
        self.probe
            .seat
            .as_ref()
            .unwrap()
            .get_touch(&self.queue.handle(), id)
    }
    fn take(&mut self) -> Vec<(usize, Event)> {
        std::mem::take(&mut self.probe.events)
    }
}

#[test]
fn cancellation_after_a_completed_frame_reaches_each_binding_once_and_drops_old_slots() {
    let mut runtime = Runtime::new();
    let mut wire = runtime.client();
    let surface = wire.surface(&mut runtime);
    let _first = wire.touch(0);
    let _second = wire.touch(1);
    wire.sync(&mut runtime);
    runtime.down(&surface, 0);
    runtime.down(&surface, 1);
    runtime.frame();
    wire.sync(&mut runtime);
    wire.take();
    runtime.cancel();
    runtime.cancel();
    wire.sync(&mut runtime);
    assert_eq!(wire.take(), [(0, Event::Cancel), (1, Event::Cancel)]);
    assert!(!runtime.touch.is_grabbed());
    let _late = wire.touch(2);
    wire.sync(&mut runtime);
    assert!(
        wire.take().is_empty(),
        "cancelled contacts were replayed to a new binding"
    );
    runtime.down(&surface, 0);
    runtime.frame();
    wire.sync(&mut runtime);
    assert_eq!(
        wire.take(),
        [
            (0, Event::Down(0)),
            (1, Event::Down(0)),
            (2, Event::Down(0)),
            (0, Event::Frame),
            (1, Event::Frame),
            (2, Event::Frame)
        ]
    );
}

#[test]
fn cancellation_includes_unchanged_contacts_and_pending_up_without_leaking_into_the_next_sequence() {
    let mut runtime = Runtime::new();
    let mut wire = runtime.client();
    let surface = wire.surface(&mut runtime);
    let _touch = wire.touch(0);
    wire.sync(&mut runtime);
    runtime.down(&surface, 0);
    runtime.down(&surface, 1);
    runtime.frame();
    wire.sync(&mut runtime);
    wire.take();
    runtime.touch.clone().up(
        &mut runtime.state,
        &UpEvent {
            slot: Some(0).into(),
            serial: 200.into(),
            time: InputTime::from_micros(2000),
        },
    );
    runtime.cancel();
    wire.sync(&mut runtime);
    assert_eq!(wire.take(), [(0, Event::Up(0)), (0, Event::Cancel)]);
    let _late = wire.touch(1);
    wire.sync(&mut runtime);
    assert!(wire.take().is_empty());
    runtime.down(&surface, 2);
    runtime.touch.clone().up(
        &mut runtime.state,
        &UpEvent {
            slot: Some(2).into(),
            serial: 202.into(),
            time: InputTime::from_micros(3000),
        },
    );
    runtime.cancel();
    wire.sync(&mut runtime);
    assert_eq!(
        wire.take(),
        [
            (0, Event::Down(2)),
            (1, Event::Down(2)),
            (0, Event::Up(2)),
            (1, Event::Up(2)),
            (0, Event::Cancel),
            (1, Event::Cancel)
        ]
    );
}

#[test]
fn moved_target_origins_apply_to_motion_and_late_binding_replay() {
    let mut runtime = Runtime::new();
    let mut wire = runtime.client();
    let surface = wire.surface(&mut runtime);
    let _first = wire.touch(0);
    wire.sync(&mut runtime);
    runtime.down(&surface, 0);
    runtime.frame();
    wire.sync(&mut runtime);
    wire.probe.coordinates.clear();
    runtime
        .touch
        .update_focus_origin(Some(0).into(), (10., 20.).into());
    let _late = wire.touch(1);
    wire.sync(&mut runtime);
    assert_eq!(std::mem::take(&mut wire.probe.coordinates), [(1, 0, -9., -19.)]);
    runtime.touch.clone().motion(
        &mut runtime.state,
        None,
        &smithay::input::touch::MotionEvent {
            slot: Some(0).into(),
            location: (13., 24.).into(),
            time: InputTime::from_micros(2000),
        },
    );
    runtime.frame();
    wire.sync(&mut runtime);
    assert_eq!(wire.probe.coordinates, [(0, 0, 3., 4.), (1, 0, 3., 4.)]);
}
