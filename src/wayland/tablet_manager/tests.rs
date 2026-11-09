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
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop, event_created_child,
    protocol::{wl_callback, wl_compositor, wl_registry, wl_seat, wl_surface},
};
use wayland_protocols::wp::{
    cursor_shape::v1::client::{
        wp_cursor_shape_device_v1 as shape, wp_cursor_shape_manager_v1 as shape_manager,
    },
    tablet::zv2::client::{
        zwp_tablet_manager_v2 as manager, zwp_tablet_pad_v2 as pad, zwp_tablet_seat_v2 as tablet_seat,
        zwp_tablet_tool_v2 as tool, zwp_tablet_v2 as tablet,
    },
};
use wayland_server::{Client, Display, backend::ClientData, protocol::wl_surface::WlSurface};

use super::*;
use crate::{
    backend::input::{InputTime, TabletToolCapabilities, TabletToolDescriptor, TabletToolType},
    input::{
        SeatState,
        tablet::{
            Tablet, TabletDescriptor, TabletSeatTrait,
            tool::{DownEvent, ProximityInEvent, ProximityOutEvent, TabletToolHandle},
        },
    },
    utils::Serial,
    wayland::{
        compositor::{CompositorClientState, CompositorHandler, CompositorState},
        cursor_shape::CursorShapeManagerState,
        pointer_constraints::PointerConstraintsHandler,
    },
};

#[derive(Debug, Default)]
struct ClientState(CompositorClientState);
impl ClientData for ClientState {}
struct State {
    compositor: CompositorState,
    seats: SeatState<Self>,
    images: Vec<crate::input::pointer::CursorImageStatus>,
}
impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;
    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seats
    }
}
impl TabletSeatHandler for State {
    type ToolFocus = WlSurface;
    fn tablet_tool_image(
        &mut self,
        _: &TabletToolDescriptor,
        image: crate::input::pointer::CursorImageStatus,
    ) {
        self.images.push(image);
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
crate::delegate_dispatch2!(State);

#[derive(Default)]
struct Probe {
    compositor: Option<wl_compositor::WlCompositor>,
    seat: Option<wl_seat::WlSeat>,
    manager: Option<manager::ZwpTabletManagerV2>,
    shapes: Option<shape_manager::WpCursorShapeManagerV1>,
    tool: Option<tool::ZwpTabletToolV2>,
    proximity: Option<u32>,
    down: Option<u32>,
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
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => probe.compositor = Some(registry.bind(name, version.min(5), qh, ())),
                "wl_seat" => probe.seat = Some(registry.bind(name, 9, qh, ())),
                "zwp_tablet_manager_v2" => probe.manager = Some(registry.bind(name, 1, qh, ())),
                "wp_cursor_shape_manager_v1" => probe.shapes = Some(registry.bind(name, 2, qh, ())),
                _ => (),
            }
        }
    }
}
impl Dispatch<tablet_seat::ZwpTabletSeatV2, ()> for Probe {
    fn event(
        probe: &mut Self,
        _: &tablet_seat::ZwpTabletSeatV2,
        event: tablet_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let tablet_seat::Event::ToolAdded { id } = event {
            probe.tool = Some(id);
        }
    }
    event_created_child!(Probe, tablet_seat::ZwpTabletSeatV2, [
        0 => (tablet::ZwpTabletV2, ()),
        1 => (tool::ZwpTabletToolV2, ()),
        2 => (pad::ZwpTabletPadV2, ())
    ]);
}
impl Dispatch<tool::ZwpTabletToolV2, ()> for Probe {
    fn event(
        probe: &mut Self,
        _: &tool::ZwpTabletToolV2,
        event: tool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            tool::Event::ProximityIn { serial, .. } => probe.proximity = Some(serial),
            tool::Event::ProximityOut => probe.proximity = None,
            tool::Event::Down { serial } => probe.down = Some(serial),
            tool::Event::Up => probe.down = None,
            _ => (),
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
delegate_noop!(Probe: manager::ZwpTabletManagerV2);
delegate_noop!(Probe: ignore tablet::ZwpTabletV2);
delegate_noop!(Probe: ignore pad::ZwpTabletPadV2);
delegate_noop!(Probe: shape_manager::WpCursorShapeManagerV1);
delegate_noop!(Probe: shape::WpCursorShapeDeviceV1);

struct Runtime {
    display: Display<State>,
    state: State,
    tablet: Tablet,
    tool: TabletToolHandle<State>,
}
impl Runtime {
    fn new() -> Self {
        let display = Display::<State>::new().unwrap();
        let compositor = CompositorState::new::<State>(&display.handle());
        TabletManagerState::new::<State>(&display.handle());
        CursorShapeManagerState::new::<State>(&display.handle());
        let mut seats = SeatState::new();
        let seat = seats.new_wl_seat(&display.handle(), "test-seat");
        let tablets = seat.tablet_seat();
        let tablet = tablets.add_wp_tablet(
            &display.handle(),
            &TabletDescriptor {
                name: "Test tablet".into(),
                usb_id: Some((0x056a, 1)),
                syspath: None,
            },
        );
        let mut state = State {
            compositor,
            seats,
            images: Vec::new(),
        };
        let tool = tablets.add_wp_tool(
            &mut state,
            &display.handle(),
            &TabletToolDescriptor {
                tool_type: TabletToolType::Pen,
                hardware_serial: 1,
                hardware_id_wacom: 0,
                capabilities: TabletToolCapabilities::PRESSURE,
            },
        );
        Self {
            display,
            state,
            tablet,
            tool,
        }
    }
    fn enter(&mut self, surface: WlSurface, serial: u32) {
        self.tool.proximity_in(
            &mut self.state,
            Some((surface, (0., 0.).into())),
            self.tablet.clone(),
            &ProximityInEvent {
                location: (1., 2.).into(),
                axis: None,
                serial: Serial::from(serial),
                time: InputTime::from_millis(1),
            },
        );
        self.tool.frame(&mut self.state, InputTime::from_millis(1));
    }
    fn down(&mut self, serial: u32) {
        self.tool.down(
            &mut self.state,
            &DownEvent {
                serial: Serial::from(serial),
                time: InputTime::from_millis(2),
            },
        );
        self.tool.frame(&mut self.state, InputTime::from_millis(2));
    }
    fn leave(&mut self, serial: u32) {
        self.tool.proximity_out(
            &mut self.state,
            &ProximityOutEvent {
                serial: Serial::from(serial),
                time: InputTime::from_millis(3),
            },
        );
        self.tool.frame(&mut self.state, InputTime::from_millis(3));
    }
}
struct Wire {
    server_client: Client,
    connection: Connection,
    queue: EventQueue<Probe>,
    probe: Probe,
    tablet_seat: Option<tablet_seat::ZwpTabletSeatV2>,
}
impl Wire {
    fn new(runtime: &mut Runtime) -> Self {
        let (server, socket) = UnixStream::pair().unwrap();
        let server_client = runtime
            .display
            .handle()
            .insert_client(server, Arc::new(ClientState::default()))
            .unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let queue = connection.new_event_queue();
        connection.display().get_registry(&queue.handle(), ());
        let mut wire = Self {
            server_client,
            connection,
            queue,
            probe: Probe::default(),
            tablet_seat: None,
        };
        wire.sync(runtime);
        wire.tablet_seat = Some(wire.probe.manager.as_ref().unwrap().get_tablet_seat(
            wire.probe.seat.as_ref().unwrap(),
            &wire.queue.handle(),
            (),
        ));
        wire.sync(runtime);
        wire
    }
    fn surface(&self) -> wl_surface::WlSurface {
        self.probe
            .compositor
            .as_ref()
            .unwrap()
            .create_surface(&self.queue.handle(), ())
    }
    fn server_surface(&self, runtime: &Runtime, surface: &wl_surface::WlSurface) -> WlSurface {
        self.server_client
            .object_from_protocol_id(&runtime.display.handle(), surface.id().protocol_id())
            .unwrap()
    }
    fn shape_device(&self) -> shape::WpCursorShapeDeviceV1 {
        self.probe.shapes.as_ref().unwrap().get_tablet_tool_v2(
            self.probe.tool.as_ref().unwrap(),
            &self.queue.handle(),
            (),
        )
    }
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
}

#[test]
fn tool_grabs_preserve_cursor_serial_and_client_authority_for_surfaces_and_shapes() {
    let mut runtime = Runtime::new();
    let mut owner = Wire::new(&mut runtime);
    let mut other = Wire::new(&mut runtime);
    let surface = owner.surface();
    let cursor = owner.surface();
    owner.sync(&mut runtime);
    let focused = owner.server_surface(&runtime, &surface);
    let cursor_surface = owner.server_surface(&runtime, &cursor);
    runtime.enter(focused.clone(), 41);
    owner.sync(&mut runtime);
    assert_eq!(owner.probe.proximity, Some(41));
    runtime.down(42);
    owner.sync(&mut runtime);
    assert_eq!(owner.probe.down, Some(42));
    assert!(runtime.tool.has_grab(42.into()));
    let shape = owner.shape_device();
    let other_shape = other.shape_device();
    let before = runtime.state.images.len();
    for serial in [0, 40, 42] {
        owner
            .probe
            .tool
            .as_ref()
            .unwrap()
            .set_cursor(serial, Some(&cursor), 3, 4);
        shape.set_shape(serial, shape::Shape::Crosshair);
    }
    owner.sync(&mut runtime);
    assert_eq!(
        runtime.state.images.len(),
        before,
        "a tip grab bypassed proximity-serial validation"
    );
    assert!(crate::wayland::compositor::get_role(&cursor_surface).is_none());
    other.probe.tool.as_ref().unwrap().set_cursor(41, None, 0, 0);
    other_shape.set_shape(41, shape::Shape::Text);
    other.sync(&mut runtime);
    assert_eq!(
        runtime.state.images.len(),
        before,
        "another client borrowed the tool grab"
    );
    owner
        .probe
        .tool
        .as_ref()
        .unwrap()
        .set_cursor(41, Some(&cursor), 3, 4);
    shape.set_shape(41, shape::Shape::Crosshair);
    owner.sync(&mut runtime);
    assert_eq!(runtime.state.images.len(), before + 2);
    runtime.leave(43);
    runtime.enter(focused, 51);
    runtime.down(52);
    owner.sync(&mut runtime);
    assert_eq!(owner.probe.proximity, Some(51));
    let before = runtime.state.images.len();
    owner.probe.tool.as_ref().unwrap().set_cursor(41, None, 0, 0);
    shape.set_shape(41, shape::Shape::Wait);
    owner.sync(&mut runtime);
    assert_eq!(
        runtime.state.images.len(),
        before,
        "an old proximity serial survived re-entry"
    );
    shape.set_shape(51, shape::Shape::Text);
    owner.sync(&mut runtime);
    assert_eq!(runtime.state.images.len(), before + 1);
    runtime.leave(53);
    owner.sync(&mut runtime);
    assert!(owner.probe.proximity.is_none());
    assert!(owner.probe.down.is_none());
    let before = runtime.state.images.len();
    owner.probe.tool.as_ref().unwrap().set_cursor(51, None, 0, 0);
    shape.set_shape(51, shape::Shape::Wait);
    owner.sync(&mut runtime);
    assert_eq!(
        runtime.state.images.len(),
        before,
        "physical proximity-out retained cursor authority"
    );
}
