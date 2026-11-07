//! Helper utilities for using x11rb as an event source in calloop.
//!
//! The primary use for this module is XWayland integration but is also widely useful for an X11
//! backend in a compositor.

use std::{
    sync::Arc,
    thread::{JoinHandle, spawn},
};

use tracing::{error, warn};
use x11rb::{connection::Connection as _, protocol::Event, rust_connection::RustConnection};

use calloop::{
    EventSource, Poll, PostAction, Readiness, Token, TokenFactory,
    channel::{Channel, ChannelError, Event as ChannelEvent, SyncSender, sync_channel},
};

/// Integration of an x11rb X11 connection with calloop.
///
/// This is a thin wrapper around `Channel`. It works by spawning an extra thread reads events from
/// the X11 connection and then sends them across the channel.
///
/// See [1] for why this extra thread is necessary. The single-thread solution proposed on that
/// page does not work with calloop, since it requires checking something on every main loop
/// iteration. Calloop only allows "when an FD becomes readable".
///
/// [1]: https://docs.rs/x11rb/0.8.1/x11rb/event_loop_integration/index.html#threads-and-races
#[derive(Debug)]
pub struct X11Source {
    connection: Arc<RustConnection>,
    channel: Option<Channel<Event>>,
    event_thread: Option<JoinHandle<()>>,
}

impl X11Source {
    /// Creates a bounded event reader. Dropping the source closes its transport
    /// and wakes the reader without waiting for the X server to respond.
    pub fn new(connection: Arc<RustConnection>) -> Self {
        let (sender, channel) = sync_channel(256);
        let conn = Arc::clone(&connection);
        let event_thread = Some(spawn(move || {
            run_event_thread(conn, sender);
        }));

        Self {
            connection,
            channel: Some(channel),
            event_thread,
        }
    }
}

impl Drop for X11Source {
    fn drop(&mut self) {
        // Signal the worker thread to exit by dropping the read end of the channel.
        self.channel.take();

        // Interrupt a blocked reader even if the X server has stopped. Dropping
        // the join handle detaches it; teardown never waits on the compositor.
        let _ = rustix::net::shutdown(self.connection.stream(), rustix::net::Shutdown::Both);
        self.event_thread.take();
    }
}

impl EventSource for X11Source {
    type Event = ChannelEvent<Event>;
    type Metadata = ();
    type Ret = ();
    type Error = ChannelError;

    #[profiling::function]
    fn process_events<C>(
        &mut self,
        readiness: Readiness,
        token: Token,
        mut callback: C,
    ) -> Result<PostAction, ChannelError>
    where
        C: FnMut(Self::Event, &mut Self::Metadata) -> Self::Ret,
    {
        if let Some(channel) = &mut self.channel {
            channel.process_events(readiness, token, move |event, meta| {
                if matches!(event, ChannelEvent::Closed) {
                    warn!("Event thread exited");
                }
                callback(event, meta)
            })
        } else {
            Ok(PostAction::Remove)
        }
    }

    fn register(&mut self, poll: &mut Poll, factory: &mut TokenFactory) -> calloop::Result<()> {
        if let Some(channel) = &mut self.channel {
            channel.register(poll, factory)?;
        }

        Ok(())
    }

    fn reregister(&mut self, poll: &mut Poll, factory: &mut TokenFactory) -> calloop::Result<()> {
        if let Some(channel) = &mut self.channel {
            channel.reregister(poll, factory)?;
        }

        Ok(())
    }

    fn unregister(&mut self, poll: &mut Poll) -> calloop::Result<()> {
        if let Some(channel) = &mut self.channel {
            channel.unregister(poll)?;
        }

        Ok(())
    }
}

/// This thread reads X11 events from the connection and sends them on the channel.
///
/// This is run in an extra thread since sending an X11 request or waiting for the reply to an X11
/// request can both read X11 events from the underlying socket which are then saved in the
/// RustConnection. Thus, readability of the underlying socket is not enough to guarantee we do not
/// miss wakeups.
///
/// This thread will call wait_for_event(). RustConnection then ensures internally to wake us up
/// when an event arrives. So far, this seems to be the only safe way to integrate x11rb with
/// calloop.
fn run_event_thread(connection: Arc<RustConnection>, sender: SyncSender<Event>) {
    loop {
        let event = match connection.wait_for_event() {
            Ok(event) => event,
            Err(err) => {
                // Connection errors are most likely permanent. Thus, exit the thread.
                error!("Event thread exiting due to connection error {}", err);
                break;
            }
        };
        match sender.send(event) {
            Ok(()) => {}
            Err(_) => {
                // The only possible error is that the other end of the channel was dropped.
                // This happens in X11Source's Drop impl.
                break;
            }
        }
    }
}
