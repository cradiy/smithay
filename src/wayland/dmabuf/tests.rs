use super::*;
use crate::{backend::allocator::Buffer, wayland::buffer::BufferHandler};
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fs::{MemfdFlags, memfd_create},
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
    protocol::{wl_callback, wl_registry},
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1 as params, zwp_linux_dmabuf_v1 as manager,
};
use wayland_server::{Display, backend::ClientData};

#[derive(Debug)]
struct ClientState;
impl ClientData for ClientState {}
struct State {
    dmabuf: DmabufState,
    imported: Vec<Fourcc>,
}
impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _: &wayland_server::protocol::wl_buffer::WlBuffer) {}
}
impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf
    }
    fn dmabuf_imported(&mut self, _: &DmabufGlobal, buffer: Dmabuf, notifier: ImportNotifier) {
        self.imported.push(buffer.format().code);
        notifier.failed();
    }
}
crate::delegate_dispatch2!(State);

#[derive(Default)]
struct Probe {
    manager: Option<manager::ZwpLinuxDmabufV1>,
    failed: usize,
}
impl Dispatch<wl_registry::WlRegistry, ()> for Probe {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        queue: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, .. } = event {
            if interface == "zwp_linux_dmabuf_v1" {
                state.manager = Some(registry.bind(name, 6, queue, ()));
            }
        }
    }
}
impl Dispatch<params::ZwpLinuxBufferParamsV1, ()> for Probe {
    fn event(
        state: &mut Self,
        _: &params::ZwpLinuxBufferParamsV1,
        event: params::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        assert!(matches!(event, params::Event::Failed));
        state.failed += 1;
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
delegate_noop!(Probe: manager::ZwpLinuxDmabufV1);
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
fn existing_parameters_accept_new_feedback_formats_and_keep_prior_advertisements_valid() {
    let mut display = Display::<State>::new().unwrap();
    let mut state = State {
        dmabuf: DmabufState::new(),
        imported: Vec::new(),
    };
    let feedback = |code| {
        DmabufFeedbackBuilder::new(
            0,
            [Format {
                code,
                modifier: Modifier::Linear,
            }],
        )
        .build()
        .unwrap()
    };
    let global = state
        .dmabuf
        .create_global_with_default_feedback::<State>(&display.handle(), &feedback(Fourcc::Argb8888));
    let (server, socket) = UnixStream::pair().unwrap();
    display
        .handle()
        .insert_client(server, Arc::new(ClientState))
        .unwrap();
    let connection = Connection::from_socket(socket).unwrap();
    let mut queue = connection.new_event_queue();
    let mut probe = Probe::default();
    connection.display().get_registry(&queue.handle(), ());
    sync(&mut display, &mut state, &connection, &mut queue, &mut probe);
    let manager = probe.manager.as_ref().unwrap();
    let old_params = manager.create_params(&queue.handle(), ());
    sync(&mut display, &mut state, &connection, &mut queue, &mut probe);
    state
        .dmabuf
        .set_default_feedback(&global, &feedback(Fourcc::Xrgb8888));
    let file = File::from(memfd_create(c"dmabuf-feedback-test", MemfdFlags::CLOEXEC).unwrap());
    file.set_len(16).unwrap();
    old_params.add(file.as_fd(), 0, 0, 16, 0, 0);
    old_params.create(4, 1, Fourcc::Xrgb8888 as u32, params::Flags::empty());
    let legacy = probe.manager.as_ref().unwrap().create_params(&queue.handle(), ());
    legacy.add(file.as_fd(), 0, 0, 16, 0, 0);
    legacy.create(4, 1, Fourcc::Argb8888 as u32, params::Flags::empty());
    sync(&mut display, &mut state, &connection, &mut queue, &mut probe);
    assert_eq!(state.imported, [Fourcc::Xrgb8888, Fourcc::Argb8888]);
    assert_eq!(
        probe.failed, 2,
        "both valid formats must reach backend import admission without a protocol error"
    );
}
