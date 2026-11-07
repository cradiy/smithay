use std::{
    io::{Read, Write},
    net::Shutdown,
    os::unix::net::{UnixListener, UnixStream},
    process::{Child, Command},
    time::{Duration, Instant},
};

use calloop::EventLoop;
use rustix::event::{PollFd, PollFlags, Timespec, poll};

use super::{AsErrno, LibSeatSession, Session, SessionEvent};

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

fn with_peer(
    test: &str,
    client: impl FnOnce(UnixStream),
    peer: impl FnOnce(&mut UnixStream, &mut UnixStream),
) {
    if let Some(path) = std::env::var_os(CHILD_SOCKET) {
        let control = UnixStream::connect(path).unwrap();
        control.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        client(control);
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
            .args([
                "--exact",
                &format!("backend::session::libseat::tests::{test}"),
                "--nocapture",
            ])
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
    peer(&mut seat, &mut control);
    assert!(child.0.wait().unwrap().success());
}

fn open_seat(seat: &mut UnixStream) {
    assert_eq!(request(seat), 1); // CLIENT_OPEN_SEAT
    let mut name = Vec::from(6u16.to_ne_bytes());
    name.extend_from_slice(b"seat0\0");
    packet(seat, SERVER | 1, &name); // SERVER_SEAT_OPENED
}

fn close_seat(seat: &mut UnixStream) {
    assert_eq!(request(seat), 2); // CLIENT_CLOSE_SEAT
    packet(seat, SERVER | 2, &[]); // SERVER_SEAT_CLOSED
}

#[test]
fn pause_completes_before_acknowledgement() {
    with_peer(
        "pause_completes_before_acknowledgement",
        |mut control| {
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
        },
        |seat, control| {
            open_seat(seat);
            marker(control, b'B');
            packet(seat, SERVER | 6, &[]); // SERVER_ENABLE_SEAT
            marker(control, b'A');
            packet(seat, SERVER | 5, &[]); // SERVER_DISABLE_SEAT

            let callback_first = {
                let mut fds = [
                    PollFd::new(&*control, PollFlags::IN),
                    PollFd::new(&*seat, PollFlags::IN),
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
                assert_eq!(request(seat), 5);
                packet(seat, SERVER | 9, &[]);
            }
            marker(control, b'P');
            control.write_all(b"R").unwrap();
            if callback_first {
                assert_eq!(request(seat), 5); // CLIENT_DISABLE_SEAT
                packet(seat, SERVER | 9, &[]); // SERVER_SEAT_DISABLED
            }
            close_seat(seat);
            assert!(
                callback_first,
                "libseat was acknowledged before the owner completed PauseSession"
            );
        },
    );
}

fn active_session(control: &mut UnixStream) -> (LibSeatSession, EventLoop<'static, Vec<SessionEvent>>) {
    let (session, notifier) = LibSeatSession::new().unwrap();
    let observed = session.clone();
    let mut writer = control.try_clone().unwrap();
    let mut event_loop = EventLoop::try_new().unwrap();
    event_loop
        .handle()
        .insert_source(notifier, move |event, _, events: &mut Vec<_>| {
            let marker = match event {
                SessionEvent::ActivateSession => {
                    assert!(observed.is_active());
                    b'A'
                }
                SessionEvent::PauseSession => {
                    assert!(!observed.is_active());
                    b'P'
                }
            };
            events.push(event);
            writer.write_all(&[marker]).unwrap();
        })
        .unwrap();
    control.write_all(b"B").unwrap();
    let mut events = Vec::new();
    while events.is_empty() {
        event_loop.dispatch(Duration::from_secs(5), &mut events).unwrap();
    }
    assert!(matches!(events.as_slice(), [SessionEvent::ActivateSession]));
    (session, event_loop)
}

fn dispatch_failure(event_loop: &mut EventLoop<'_, Vec<SessionEvent>>) -> super::Error {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut events = Vec::new();
    loop {
        if let Err(error) = event_loop.dispatch(Duration::from_millis(100), &mut events) {
            assert!(matches!(events.as_slice(), [SessionEvent::PauseSession]));
            let calloop::Error::OtherError(error) = error else {
                panic!("unexpected event loop error: {error}");
            };
            return *error.downcast::<super::Error>().unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "seat failure was not reported to its owner"
        );
    }
}

fn activate(seat: &mut UnixStream, control: &mut UnixStream) {
    open_seat(seat);
    marker(control, b'B');
    packet(seat, SERVER | 6, &[]);
    marker(control, b'A');
}

#[test]
fn service_disconnect_pauses_the_owner_and_returns_an_error() {
    with_peer(
        "service_disconnect_pauses_the_owner_and_returns_an_error",
        |mut control| {
            let (session, mut event_loop) = active_session(&mut control);
            let error = dispatch_failure(&mut event_loop);
            assert!(error.as_errno().is_some());
            assert!(!session.is_active());
        },
        |seat, control| {
            activate(seat, control);
            seat.shutdown(Shutdown::Both).unwrap();
            marker(control, b'P');
        },
    );
}

#[test]
fn dispatch_error_pauses_the_owner_and_preserves_errno() {
    with_peer(
        "dispatch_error_pauses_the_owner_and_preserves_errno",
        |mut control| {
            let (session, mut event_loop) = active_session(&mut control);
            let error = dispatch_failure(&mut event_loop);
            assert_eq!(error.as_errno(), Some(rustix::io::Errno::BADMSG.raw_os_error()));
            assert!(!session.is_active());
        },
        |seat, control| {
            activate(seat, control);
            packet(seat, SERVER | 8, &[]); // A reply without a pending switch request.
            marker(control, b'P');
        },
    );
}

#[test]
fn disable_error_is_reported_after_owner_suspension() {
    with_peer(
        "disable_error_is_reported_after_owner_suspension",
        |mut control| {
            let (mut session, mut event_loop) = active_session(&mut control);
            let error = dispatch_failure(&mut event_loop);
            assert_eq!(error.as_errno(), Some(rustix::io::Errno::ACCESS.raw_os_error()));
            assert!(!session.is_active());
            assert!(matches!(
                session.open(std::path::Path::new("/dev/dri/test"), rustix::fs::OFlags::RDWR),
                Err(super::Error::SessionLost)
            ));
            control.write_all(b"E").unwrap();
            marker(&mut control, b'R');
            let mut events = Vec::new();
            let error = event_loop
                .dispatch(Duration::from_secs(5), &mut events)
                .unwrap_err();
            let calloop::Error::OtherError(error) = error else {
                panic!("unexpected event loop error: {error}");
            };
            assert!(matches!(
                *error.downcast::<super::Error>().unwrap(),
                super::Error::SessionLost
            ));
            assert!(events.is_empty());
            assert!(!session.is_active());
        },
        |seat, control| {
            activate(seat, control);
            packet(seat, SERVER | 5, &[]);
            marker(control, b'P');
            assert_eq!(request(seat), 5);
            packet(
                seat,
                u16::MAX,
                &rustix::io::Errno::ACCESS.raw_os_error().to_ne_bytes(),
            );
            marker(control, b'E');
            packet(seat, SERVER | 6, &[]);
            control.write_all(b"R").unwrap();
            close_seat(seat);
        },
    );
}

#[test]
fn startup_dispatch_error_is_returned_without_panicking() {
    with_peer(
        "startup_dispatch_error_is_returned_without_panicking",
        |_control| {
            assert!(
                matches!(LibSeatSession::new(), Err(super::Error::FailedToOpenSession(errno))
            if errno == rustix::io::Errno::BADMSG)
            );
        },
        |seat, _control| {
            assert_eq!(request(seat), 1);
            // One write keeps the unsolicited reply in libseat's input buffer
            // when the open-seat response is consumed.
            let mut reply = Vec::from((SERVER | 1).to_ne_bytes());
            reply.extend_from_slice(&8u16.to_ne_bytes());
            reply.extend_from_slice(&6u16.to_ne_bytes());
            reply.extend_from_slice(b"seat0\0");
            reply.extend_from_slice(&(SERVER | 8).to_ne_bytes());
            reply.extend_from_slice(&0u16.to_ne_bytes());
            seat.write_all(&reply).unwrap();
        },
    );
}
