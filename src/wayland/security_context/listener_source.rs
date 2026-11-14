use calloop::{
    EventSource, Interest, Mode, Poll, PostAction, Readiness, Token, TokenFactory, generic::Generic,
};
use rustix::event::{Timespec, epoll};
use std::{
    io,
    os::{
        fd::OwnedFd,
        unix::net::{UnixListener, UnixStream},
    },
};

/// Security context listener event source.
///
/// Accepts one connection per readiness event, and retires on close_fd hangup.
#[derive(Debug)]
pub struct SecurityContextListenerSource {
    listen_fd: UnixListener,
    _close_fd: OwnedFd,
    readiness: Generic<OwnedFd>,
}

impl SecurityContextListenerSource {
    pub(super) fn new(listen_fd: UnixListener, close_fd: OwnedFd) -> io::Result<Self> {
        listen_fd.set_nonblocking(true)?;
        let poll = epoll::create(epoll::CreateFlags::CLOEXEC)?;
        epoll::add(
            &poll,
            &listen_fd,
            epoll::EventData::new_u64(0),
            epoll::EventFlags::IN,
        )?;
        // HUP is reported unconditionally. RDHUP also handles a socket peer's
        // write shutdown, without treating ordinary readable data as closure.
        epoll::add(
            &poll,
            &close_fd,
            epoll::EventData::new_u64(1),
            epoll::EventFlags::RDHUP,
        )?;
        Ok(Self {
            listen_fd,
            _close_fd: close_fd,
            readiness: Generic::new(poll, Interest::READ, Mode::Level),
        })
    }
}

impl EventSource for SecurityContextListenerSource {
    type Event = UnixStream;
    type Metadata = ();
    type Ret = ();
    type Error = io::Error;

    fn process_events<F: FnMut(Self::Event, &mut ())>(
        &mut self,
        readiness: Readiness,
        token: Token,
        mut callback: F,
    ) -> io::Result<PostAction> {
        let listener = &self.listen_fd;
        self.readiness.process_events(readiness, token, |_, poll| {
            let mut events = [epoll::Event {
                flags: epoll::EventFlags::empty(),
                data: epoll::EventData::new_u64(0),
            }; 2];
            let count = match epoll::wait(&**poll, &mut events, Some(&Timespec::default())) {
                Err(rustix::io::Errno::INTR) => return Ok(PostAction::Continue),
                result => result?,
            };
            let events = &events[..count];
            // Revoke before accepting, including when both descriptors are ready.
            if events.iter().any(|event| event.data.u64() == 1) {
                return Ok(PostAction::Remove);
            }
            if events.iter().any(|event| event.data.u64() == 0) {
                match listener.accept() {
                    Ok((stream, _)) => callback(stream, &mut ()),
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock
                                | io::ErrorKind::Interrupted
                                | io::ErrorKind::ConnectionAborted
                        ) =>
                    {
                        ()
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(PostAction::Continue)
        })
    }
    fn register(&mut self, poll: &mut Poll, factory: &mut TokenFactory) -> calloop::Result<()> {
        self.readiness.register(poll, factory)
    }
    fn reregister(&mut self, poll: &mut Poll, factory: &mut TokenFactory) -> calloop::Result<()> {
        self.readiness.reregister(poll, factory)
    }
    fn unregister(&mut self, poll: &mut Poll) -> calloop::Result<()> {
        self.readiness.unregister(poll)
    }
}
