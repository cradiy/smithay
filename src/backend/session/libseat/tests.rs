use std::{
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    process::{Child, Command},
    time::Duration,
};

use calloop::EventLoop;
use rustix::event::{PollFd, PollFlags, Timespec, poll};

use super::{LibSeatSession, Session, SessionEvent};

const TEST: &str = "backend::session::libseat::tests::pause_completes_before_acknowledgement";
const CHILD_SOCKET: &str = "SMITHAY_LIBSEAT_TEST_CONTROL";
const SERVER: u16 = 1 << 15;

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn packet(stream: &mut UnixStream, opcode: u16, payload: &[u8]) {
    let mut bytes = Vec::from(opcode.to_ne_bytes());
    bytes.extend_from_slice(&(payload.len() as u16).to_ne_bytes());
    bytes.extend_from_slice(payload);
    stream.write_all(&bytes).unwrap();
}

fn request(stream: &mut UnixStream) -> u16 {
    let mut header = [0; 4];
    stream.read_exact(&mut header).unwrap();
    assert_eq!(u16::from_ne_bytes([header[2], header[3]]), 0);
    u16::from_ne_bytes([header[0], header[1]])
}

fn marker(stream: &mut UnixStream, expected: u8) {
    let mut byte = [0];
    stream.read_exact(&mut byte).unwrap();
    assert_eq!(byte[0], expected);
}

#[test]
fn pause_completes_before_acknowledgement() {
    if let Some(path) = std::env::var_os(CHILD_SOCKET) {
        let mut control = UnixStream::connect(path).unwrap();
        let (session, notifier) = LibSeatSession::new().unwrap();
        assert!(!session.is_active());
        let mut event_loop = EventLoop::<usize>::try_new().unwrap();
        control.write_all(b"B").unwrap();
        event_loop
            .handle()
            .insert_source(notifier, |event, _, count| {
                match event {
                    SessionEvent::ActivateSession => {
                        assert!(session.is_active());
                        control.write_all(b"A").unwrap();
                    }
                    SessionEvent::PauseSession => {
                        assert!(!session.is_active());
                        control.write_all(b"P").unwrap();
                        // Stand in for the owner's synchronous input/DRM suspension.
                        marker(&mut control, b'R');
                    }
                }
                *count += 1;
            })
            .unwrap();
        let mut count = 0;
        while count < 2 {
            event_loop.dispatch(Duration::from_secs(5), &mut count).unwrap();
        }
        drop(event_loop);
        assert!(!session.is_active());
        return;
    }

    // Exercise the installed C libseat through a private seatd protocol peer.
    // Neither the real seat service nor a physical device is contacted.
    let directory = tempfile::tempdir().unwrap();
    let seat_path = directory.path().join("seat.sock");
    let control_path = directory.path().join("control.sock");
    let seat_listener = UnixListener::bind(&seat_path).unwrap();
    let control_listener = UnixListener::bind(&control_path).unwrap();
    let mut child = ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env("LIBSEAT_BACKEND", "seatd")
            .env("SEATD_SOCK", &seat_path)
            .env(CHILD_SOCKET, &control_path)
            .spawn()
            .unwrap(),
    );
    let (mut control, _) = control_listener.accept().unwrap();
    let (mut seat, _) = seat_listener.accept().unwrap();
    for stream in [&control, &seat] {
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    }
    assert_eq!(request(&mut seat), 1); // CLIENT_OPEN_SEAT
    let mut name = Vec::from(6u16.to_ne_bytes());
    name.extend_from_slice(b"seat0\0");
    packet(&mut seat, SERVER | 1, &name); // SERVER_SEAT_OPENED
    marker(&mut control, b'B');
    packet(&mut seat, SERVER | 6, &[]); // SERVER_ENABLE_SEAT
    marker(&mut control, b'A');
    packet(&mut seat, SERVER | 5, &[]); // SERVER_DISABLE_SEAT

    let callback_first = {
        let mut fds = [
            PollFd::new(&control, PollFlags::IN),
            PollFd::new(&seat, PollFlags::IN),
        ];
        poll(
            &mut fds,
            Some(&Timespec {
                tv_sec: 5,
                tv_nsec: 0,
            }),
        )
        .unwrap();
        fds[0].revents().contains(PollFlags::IN) && !fds[1].revents().contains(PollFlags::IN)
    };
    if !callback_first {
        // Drain the old implementation's early acknowledgement before failing.
        assert_eq!(request(&mut seat), 5);
        packet(&mut seat, SERVER | 9, &[]);
    }
    marker(&mut control, b'P');
    control.write_all(b"R").unwrap();
    if callback_first {
        assert_eq!(request(&mut seat), 5); // CLIENT_DISABLE_SEAT
        packet(&mut seat, SERVER | 9, &[]); // SERVER_SEAT_DISABLED
    }
    assert_eq!(request(&mut seat), 2); // CLIENT_CLOSE_SEAT
    packet(&mut seat, SERVER | 2, &[]); // SERVER_SEAT_CLOSED
    assert!(child.0.wait().unwrap().success());
    assert!(
        callback_first,
        "libseat was acknowledged before the owner completed PauseSession"
    );
}
