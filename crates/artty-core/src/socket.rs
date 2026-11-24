//! Reading a Unix socket without trusting a blocked read to wake.
//!
//! macOS (seen on 26.5) can leave a thread asleep in `recv` on a stream
//! socket after the other end shut it down, when this side was writing to it
//! at the time: the end is there (a peek returns 0) but the sleeper is never
//! woken. A Python reproduction (one end reading and writing, the other
//! writing 157 bytes then shutting down) did so about once in a thousand
//! times. For the server it meant an app that had gone was never seen to go,
//! and its `artty server --stdio` waited on it forever.
//!
//! So reads wait in `poll`, a second at a time, which looks afresh each time.

use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

/// The longest a missed wake-up delays seeing what arrived (or the end).
const RECHECK_MS: i32 = 1000;

/// A Unix socket, read as a stream (see the module).
pub struct Reader(pub UnixStream);

impl Reader {
    pub fn get_ref(&self) -> &UnixStream {
        &self.0
    }
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let mut fd = libc::pollfd { fd: self.0.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd.
            let ready = unsafe { libc::poll(&mut fd, 1, RECHECK_MS) };
            if ready < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if ready > 0 {
                // Readable, ended or failed: none of these block.
                return self.0.read(buf);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_what_comes_then_the_end() {
        let (mut ours, theirs) = UnixStream::pair().unwrap();
        let mut reader = Reader(theirs);
        let sender = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            ours.write_all(b"hello").unwrap();
            ours.shutdown(std::net::Shutdown::Write).unwrap();
        });
        let mut got = Vec::new();
        reader.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"hello");
        sender.join().unwrap();
    }
}
