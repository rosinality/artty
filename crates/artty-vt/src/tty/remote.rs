//! artty: a terminal whose program runs elsewhere (a shell over ssh), not on
//! a PTY here. Its bytes come and go through one end of a socket pair; the
//! other end is the carrier's (which copies them to and from the program).
//! Sizes go to a callback, and the carrier says the program ended by writing
//! to `ended`'s other end.

use std::io::{ErrorKind, Read, Result};
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use polling::{Event, PollMode, Poller};

use crate::event::{OnResize, WindowSize};
use crate::tty::{
    ChildEvent, EventedPty, EventedReadWrite, PTY_CHILD_EVENT_TOKEN, PTY_READ_WRITE_TOKEN,
};

pub struct Remote {
    stream: UnixStream,
    ended: UnixStream,
    resize: Box<dyn FnMut(WindowSize) + Send>,
}

/// The carrier's ends of a [`Remote`].
pub struct RemoteEnds {
    /// What the terminal sends the program, and what the program shows.
    pub stream: UnixStream,
    /// Written to (or dropped) when the program has ended.
    pub ended: UnixStream,
}

impl Remote {
    pub fn new(resize: Box<dyn FnMut(WindowSize) + Send>) -> Result<(Remote, RemoteEnds)> {
        let (stream, theirs) = UnixStream::pair()?;
        let (ended, end) = UnixStream::pair()?;
        stream.set_nonblocking(true)?;
        ended.set_nonblocking(true)?;
        Ok((Remote { stream, ended, resize }, RemoteEnds { stream: theirs, ended: end }))
    }
}

impl EventedReadWrite for Remote {
    type Reader = UnixStream;
    type Writer = UnixStream;

    unsafe fn register(
        &mut self,
        poll: &Arc<Poller>,
        mut interest: Event,
        mode: PollMode,
    ) -> Result<()> {
        interest.key = PTY_READ_WRITE_TOKEN;
        unsafe {
            poll.add_with_mode(&self.stream, interest, mode)?;
            poll.add_with_mode(&self.ended, Event::readable(PTY_CHILD_EVENT_TOKEN), PollMode::Level)
        }
    }

    fn reregister(
        &mut self,
        poll: &Arc<Poller>,
        mut interest: Event,
        mode: PollMode,
    ) -> Result<()> {
        interest.key = PTY_READ_WRITE_TOKEN;
        poll.modify_with_mode(&self.stream, interest, mode)?;
        poll.modify_with_mode(&self.ended, Event::readable(PTY_CHILD_EVENT_TOKEN), PollMode::Level)
    }

    fn deregister(&mut self, poll: &Arc<Poller>) -> Result<()> {
        poll.delete(&self.stream)?;
        poll.delete(&self.ended)
    }

    fn reader(&mut self) -> &mut UnixStream {
        &mut self.stream
    }

    fn writer(&mut self) -> &mut UnixStream {
        &mut self.stream
    }
}

impl EventedPty for Remote {
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        // A byte, or the other end gone: it ended.
        match self.ended.read(&mut [0]) {
            Err(err) if err.kind() == ErrorKind::WouldBlock => None,
            _ => Some(ChildEvent::Exited(None)),
        }
    }
}

impl OnResize for Remote {
    fn on_resize(&mut self, size: WindowSize) {
        (self.resize)(size);
    }
}
