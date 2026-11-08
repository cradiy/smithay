#![cfg(feature = "wayland_frontend")]

use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fs::{MemfdFlags, memfd_create},
};
use smithay::{
    input::{SeatHandler, SeatState},
    output::{Output, PhysicalProperties, Subpixel},
    reexports::wayland_server::{Display, backend::ClientData, protocol::wl_surface::WlSurface},
    utils::{Rectangle, Transform},
    wayland::{
        buffer::BufferHandler,
        compositor::{CompositorClientState, CompositorHandler, CompositorState},
        image_capture_source::{
            ImageCaptureSource, ImageCaptureSourceHandler, OutputCaptureSourceHandler,
            OutputCaptureSourceState,
        },
        image_copy_capture::{
            BufferConstraints, Frame, ImageCopyCaptureHandler, ImageCopyCaptureState, Session, SessionRef,
        },
        output::{OutputHandler, OutputManagerState},
        pointer_constraints::PointerConstraintsHandler,
        shm::{ShmHandler, ShmState},
    },
};
use std::{
    fs::File,
    os::{fd::AsFd, unix::net::UnixStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, delegate_noop,
    protocol::{wl_buffer, wl_callback, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool},
};
use wayland_protocols::ext::{
    image_capture_source::v1::client::{
        ext_image_capture_source_v1 as source, ext_output_image_capture_source_manager_v1 as sources,
    },
    image_copy_capture::v1::client::{
        ext_image_copy_capture_cursor_session_v1 as cursor, ext_image_copy_capture_frame_v1 as frame,
        ext_image_copy_capture_manager_v1 as manager, ext_image_copy_capture_session_v1 as session,
    },
};

#[derive(Debug)]
struct ClientState(CompositorClientState);
impl ClientData for ClientState {}
struct State {
    compositor: CompositorState,
    source: OutputCaptureSourceState,
    capture: ImageCopyCaptureState,
    shm: ShmState,
    seats: SeatState<Self>,
    sessions: Vec<Session>,
    frames: Vec<(SessionRef, Frame)>,
    reject: bool,
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
    fn client_compositor_state<'a>(
        &self,
        client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().0
    }
    fn commit(&mut self, _: &WlSurface) {}
}
impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer) {}
}
impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}
impl OutputHandler for State {}
impl ImageCaptureSourceHandler for State {}
impl OutputCaptureSourceHandler for State {
    fn output_capture_source_state(&mut self) -> &mut OutputCaptureSourceState {
        &mut self.source
    }
}
impl ImageCopyCaptureHandler for State {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.capture
    }
    fn capture_constraints(&mut self, _: &ImageCaptureSource) -> Option<BufferConstraints> {
        if self.reject {
            return None;
        }
        Some(BufferConstraints {
            size: (2, 2).into(),
            shm: vec![smithay::reexports::wayland_server::protocol::wl_shm::Format::Argb8888],
            #[cfg(feature = "backend_drm")]
            dma: None,
        })
    }
    fn new_session(&mut self, session: Session) {
        self.sessions.push(session);
    }
    fn frame(&mut self, session: &SessionRef, frame: Frame) {
        self.frames.push((session.clone(), frame));
    }
    fn session_destroyed(&mut self, session: SessionRef) {
        self.sessions.retain(|s| *s != session);
    }
}
smithay::delegate_dispatch2!(State);

struct Runtime {
    display: Display<State>,
    state: State,
    _output: Output,
}
impl Runtime {
    fn new() -> Self {
        let display = Display::new().unwrap();
        let dh = display.handle();
        let mut seats = SeatState::new();
        seats.new_wl_seat(&dh, "capture-test").add_pointer();
        OutputManagerState::new();
        let output = Output::new(
            "test".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Test".into(),
                model: "Test".into(),
                serial_number: String::new(),
            },
        );
        output.create_global::<State>(&dh);
        let state = State {
            compositor: CompositorState::new::<State>(&dh),
            source: OutputCaptureSourceState::new::<State>(&dh),
            capture: ImageCopyCaptureState::new::<State>(&dh),
            shm: ShmState::new::<State>(&dh, []),
            seats,
            sessions: Vec::new(),
            frames: Vec::new(),
            reject: false,
        };
        Self {
            display,
            state,
            _output: output,
        }
    }
    fn dispatch(&mut self) {
        self.display.dispatch_clients(&mut self.state).unwrap();
        self.state.capture.cleanup();
        self.display.flush_clients().unwrap();
    }
}
#[derive(Default)]
struct Probe {
    source: Option<sources::ExtOutputImageCaptureSourceManagerV1>,
    manager: Option<manager::ExtImageCopyCaptureManagerV1>,
    output: Option<wl_output::WlOutput>,
    shm: Option<wl_shm::WlShm>,
    pointer: Option<wl_pointer::WlPointer>,
    stopped: usize,
    ready: usize,
    failed: Vec<frame::FailureReason>,
}
impl Dispatch<wl_registry::WlRegistry, ()> for Probe {
    fn event(
        s: &mut Self,
        r: &wl_registry::WlRegistry,
        e: wl_registry::Event,
        _: &(),
        _: &Connection,
        q: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = e
        {
            match interface.as_str() {
                "ext_output_image_capture_source_manager_v1" => s.source = Some(r.bind(name, 1, q, ())),
                "ext_image_copy_capture_manager_v1" => s.manager = Some(r.bind(name, 1, q, ())),
                "wl_output" => s.output = Some(r.bind(name, version.min(4), q, ())),
                "wl_shm" => s.shm = Some(r.bind(name, version.min(2), q, ())),
                "wl_seat" => {
                    let seat: wl_seat::WlSeat = r.bind(name, version.min(9), q, ());
                    s.pointer = Some(seat.get_pointer(q, ()));
                }
                _ => (),
            }
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
impl Dispatch<session::ExtImageCopyCaptureSessionV1, ()> for Probe {
    fn event(
        s: &mut Self,
        _: &session::ExtImageCopyCaptureSessionV1,
        e: session::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(e, session::Event::Stopped) {
            s.stopped += 1;
        }
    }
}
impl Dispatch<frame::ExtImageCopyCaptureFrameV1, ()> for Probe {
    fn event(
        s: &mut Self,
        _: &frame::ExtImageCopyCaptureFrameV1,
        e: frame::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            frame::Event::Ready => s.ready += 1,
            frame::Event::Failed { reason } => s.failed.push(reason.into_result().unwrap()),
            _ => (),
        }
    }
}
delegate_noop!(Probe: ignore manager::ExtImageCopyCaptureManagerV1);
delegate_noop!(Probe: ignore sources::ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(Probe: ignore source::ExtImageCaptureSourceV1);
delegate_noop!(Probe: ignore cursor::ExtImageCopyCaptureCursorSessionV1);
delegate_noop!(Probe: ignore wl_output::WlOutput);
delegate_noop!(Probe: ignore wl_seat::WlSeat);
delegate_noop!(Probe: ignore wl_pointer::WlPointer);
delegate_noop!(Probe: ignore wl_shm::WlShm);
delegate_noop!(Probe: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Probe: ignore wl_buffer::WlBuffer);
struct Wire {
    connection: Connection,
    queue: EventQueue<Probe>,
    probe: Probe,
}
impl Wire {
    fn new(rt: &mut Runtime) -> Self {
        let (server, client) = UnixStream::pair().unwrap();
        rt.display
            .handle()
            .insert_client(server, Arc::new(ClientState(CompositorClientState::default())))
            .unwrap();
        let connection = Connection::from_socket(client).unwrap();
        let queue = connection.new_event_queue();
        connection.display().get_registry(&queue.handle(), ());
        let mut wire = Self {
            connection,
            queue,
            probe: Probe::default(),
        };
        wire.sync(rt);
        wire
    }
    fn sync(&mut self, rt: &mut Runtime) {
        let done = Arc::new(AtomicBool::new(false));
        self.connection.display().sync(&self.queue.handle(), done.clone());
        let deadline = Instant::now() + Duration::from_secs(2);
        while !done.load(Ordering::Relaxed) {
            self.connection.flush().unwrap();
            rt.dispatch();
            self.queue.dispatch_pending(&mut self.probe).unwrap();
            if done.load(Ordering::Relaxed) {
                break;
            }
            if let Some(read) = self.queue.prepare_read() {
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
    }
    fn session(
        &mut self,
        rt: &mut Runtime,
    ) -> (
        source::ExtImageCaptureSourceV1,
        session::ExtImageCopyCaptureSessionV1,
    ) {
        let q = self.queue.handle();
        let source =
            self.probe
                .source
                .as_ref()
                .unwrap()
                .create_source(self.probe.output.as_ref().unwrap(), &q, ());
        let session =
            self.probe
                .manager
                .as_ref()
                .unwrap()
                .create_session(&source, manager::Options::empty(), &q, ());
        self.sync(rt);
        (source, session)
    }
    fn buffer(&self) -> (File, wl_buffer::WlBuffer) {
        let file = File::from(memfd_create(c"capture", MemfdFlags::CLOEXEC).unwrap());
        file.set_len(16).unwrap();
        let q = self.queue.handle();
        let pool = self
            .probe
            .shm
            .as_ref()
            .unwrap()
            .create_pool(file.as_fd(), 16, &q, ());
        let buffer = pool.create_buffer(0, 2, 2, 8, wl_shm::Format::Argb8888, &q, ());
        pool.destroy();
        (file, buffer)
    }
    fn error(&mut self, rt: &mut Runtime) -> wayland_client::backend::protocol::ProtocolError {
        self.connection.flush().unwrap();
        rt.dispatch();
        let read = self.queue.prepare_read().unwrap();
        assert!(read.read().is_err());
        self.connection
            .protocol_error()
            .expect("request must be a protocol error")
    }
}

#[test]
fn invalid_capture_requests_are_protocol_errors_and_leave_other_clients_usable() {
    for case in 0..8 {
        let mut rt = Runtime::new();
        let mut a = Wire::new(&mut rt);
        let mut healthy = Wire::new(&mut rt);
        let (source, session) = a.session(&mut rt);
        let (_file, buffer) = a.buffer();
        let q = a.queue.handle();
        let frame = session.create_frame(&q, ());
        let (interface, code) = match case {
            0 => {
                frame.capture();
                ("ext_image_copy_capture_frame_v1", frame::Error::NoBuffer as u32)
            }
            1 => {
                frame.damage_buffer(-1, 0, 1, 1);
                (
                    "ext_image_copy_capture_frame_v1",
                    frame::Error::InvalidBufferDamage as u32,
                )
            }
            2 => {
                frame.attach_buffer(&buffer);
                frame.capture();
                frame.capture();
                (
                    "ext_image_copy_capture_frame_v1",
                    frame::Error::AlreadyCaptured as u32,
                )
            }
            3 => {
                frame.attach_buffer(&buffer);
                frame.capture();
                frame.attach_buffer(&buffer);
                (
                    "ext_image_copy_capture_frame_v1",
                    frame::Error::AlreadyCaptured as u32,
                )
            }
            4 => {
                frame.attach_buffer(&buffer);
                frame.capture();
                frame.damage_buffer(0, 0, 1, 1);
                (
                    "ext_image_copy_capture_frame_v1",
                    frame::Error::AlreadyCaptured as u32,
                )
            }
            5 => {
                a.probe.manager.as_ref().unwrap().create_session(
                    &source,
                    manager::Options::from_bits_retain(2),
                    &q,
                    (),
                );
                (
                    "ext_image_copy_capture_manager_v1",
                    manager::Error::InvalidOption as u32,
                )
            }
            6 => {
                let cursor = a.probe.manager.as_ref().unwrap().create_pointer_cursor_session(
                    &source,
                    a.probe.pointer.as_ref().unwrap(),
                    &q,
                    (),
                );
                cursor.get_capture_session(&q, ());
                cursor.get_capture_session(&q, ());
                (
                    "ext_image_copy_capture_cursor_session_v1",
                    cursor::Error::DuplicateSession as u32,
                )
            }
            7 => {
                a.sync(&mut rt);
                rt.state.sessions.pop().unwrap().stop();
                session.create_frame(&q, ());
                (
                    "ext_image_copy_capture_session_v1",
                    session::Error::DuplicateFrame as u32,
                )
            }
            _ => unreachable!(),
        };
        let error = a.error(&mut rt);
        assert_eq!(error.object_interface, interface, "case {case}");
        assert_eq!(error.code, code, "case {case}");
        healthy.sync(&mut rt);
    }
}

#[test]
fn stopped_frames_fail_once_even_when_stopped_before_capture_and_remain_exclusive_until_destroyed() {
    let mut rt = Runtime::new();
    let mut a = Wire::new(&mut rt);
    let (_source, session) = a.session(&mut rt);
    let (_file, buffer) = a.buffer();
    let q = a.queue.handle();
    let first = session.create_frame(&q, ());
    a.sync(&mut rt);
    rt.state.sessions.pop().unwrap().stop();
    a.sync(&mut rt);
    assert_eq!(a.probe.stopped, 1);
    first.attach_buffer(&buffer);
    first.capture();
    a.sync(&mut rt);
    assert_eq!(a.probe.failed, [frame::FailureReason::Stopped]);
    assert!(rt.state.frames.is_empty());
    first.destroy();
    a.sync(&mut rt);
    let second = session.create_frame(&q, ());
    second.attach_buffer(&buffer);
    second.capture();
    a.sync(&mut rt);
    assert_eq!(
        a.probe.failed,
        [frame::FailureReason::Stopped, frame::FailureReason::Stopped]
    );
    session.create_frame(&q, ());
    let error = a.error(&mut rt);
    assert_eq!(error.code, session::Error::DuplicateFrame as u32);
}

#[test]
fn destroying_source_and_parent_session_preserves_an_existing_uncaptured_frame() {
    let mut rt = Runtime::new();
    let mut a = Wire::new(&mut rt);
    let (source, session) = a.session(&mut rt);
    let (_file, buffer) = a.buffer();
    let q = a.queue.handle();
    let frame = session.create_frame(&q, ());
    source.destroy();
    session.destroy();
    a.sync(&mut rt);
    frame.attach_buffer(&buffer);
    frame.capture();
    a.sync(&mut rt);
    let (_, pending) = rt
        .state
        .frames
        .pop()
        .expect("frame must remain independent of its parent resource");
    pending.success(
        Transform::Normal,
        Some(vec![Rectangle::from_size((2, 2).into())]),
        Duration::from_secs(1),
    );
    a.sync(&mut rt);
    assert_eq!(a.probe.ready, 1);
    assert!(a.probe.failed.is_empty());
    frame.destroy();
    a.sync(&mut rt);
    assert!(rt.state.capture.sessions().is_empty());
}

#[test]
fn rejected_output_and_cursor_sessions_release_their_frame_slots_after_destruction() {
    for cursor_capture in [false, true] {
        let mut rt = Runtime::new();
        let mut a = Wire::new(&mut rt);
        let (source, _unused) = a.session(&mut rt);
        let (_file, buffer) = a.buffer();
        let q = a.queue.handle();
        let session = if cursor_capture {
            let cursor = a.probe.manager.as_ref().unwrap().create_pointer_cursor_session(
                &source,
                a.probe.pointer.as_ref().unwrap(),
                &q,
                (),
            );
            cursor.get_capture_session(&q, ())
        } else {
            rt.state.reject = true;
            a.probe
                .manager
                .as_ref()
                .unwrap()
                .create_session(&source, manager::Options::empty(), &q, ())
        };
        a.sync(&mut rt);
        assert_eq!(a.probe.stopped, 1);
        for count in 1..=2 {
            let frame = session.create_frame(&q, ());
            frame.attach_buffer(&buffer);
            frame.capture();
            a.sync(&mut rt);
            assert_eq!(a.probe.failed, vec![frame::FailureReason::Stopped; count]);
            frame.destroy();
            a.sync(&mut rt);
        }
        assert!(rt.state.frames.is_empty());
    }
}

#[test]
fn explicit_owner_stop_revokes_an_orphaned_uncaptured_frame() {
    let mut rt = Runtime::new();
    let mut a = Wire::new(&mut rt);
    let (_source, session) = a.session(&mut rt);
    let owner = rt.state.sessions.pop().unwrap();
    let (_file, buffer) = a.buffer();
    let frame = session.create_frame(&a.queue.handle(), ());
    session.destroy();
    a.sync(&mut rt);
    owner.stop();
    frame.attach_buffer(&buffer);
    frame.capture();
    a.sync(&mut rt);
    assert_eq!(a.probe.failed, [frame::FailureReason::Stopped]);
    assert!(rt.state.frames.is_empty());
}

#[test]
fn destroying_an_inflight_frame_cancels_its_late_completion_and_releases_the_slot() {
    let mut rt = Runtime::new();
    let mut a = Wire::new(&mut rt);
    let (_source, session) = a.session(&mut rt);
    let (_file, buffer) = a.buffer();
    let q = a.queue.handle();
    let frame = session.create_frame(&q, ());
    frame.attach_buffer(&buffer);
    frame.capture();
    a.sync(&mut rt);
    let (_, inflight) = rt.state.frames.pop().unwrap();
    frame.destroy();
    a.sync(&mut rt);
    assert!(inflight.has_failed());
    inflight.success(Transform::Normal, None, Duration::from_secs(1));
    let next = session.create_frame(&q, ());
    next.attach_buffer(&buffer);
    next.capture();
    a.sync(&mut rt);
    assert_eq!(a.probe.ready, 0);
    assert!(a.probe.failed.is_empty());
    let (_, pending) = rt.state.frames.pop().unwrap();
    pending.success(Transform::Normal, None, Duration::from_secs(2));
    a.sync(&mut rt);
    assert_eq!(a.probe.ready, 1);
}
