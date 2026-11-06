//! What carries a connection to a server: two ends, read and written as
//! plain blocking streams (whatever is underneath — ssh built in, a command
//! whose stdin and stdout reach `artty server --stdio`, or the server's
//! socket), and what keeps it open.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
#[cfg(feature = "ssh")]
use std::sync::mpsc;

/// A connection's two ends. Dropping it closes the connection.
pub struct Link {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    /// What it runs on (a process, an ssh session and its runtime): ended
    /// when dropped.
    pub(crate) keep: Box<dyn Send>,
}

impl Link {
    /// A command reaching the server: `artty server --stdio` itself (a
    /// server on this machine), or what runs it elsewhere.
    pub fn command(command: &[String]) -> Result<Link, String> {
        let (program, args) = command.split_first().ok_or("no command")?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|err| format!("{program}: {err}"))?;
        let writer = child.stdin.take().ok_or("no stdin")?;
        let reader = child.stdout.take().ok_or("no stdout")?;
        Ok(Link {
            reader: Box::new(reader),
            writer: Box::new(writer),
            keep: Box::new(Ended(child)),
        })
    }
}

impl Link {
    /// The server's socket, connected (on this machine).
    pub fn socket(stream: UnixStream) -> Result<Link, String> {
        let writer = stream.try_clone().map_err(|err| err.to_string())?;
        Ok(Link {
            reader: Box::new(stream.try_clone().map_err(|err| err.to_string())?),
            writer: Box::new(writer),
            keep: Box::new(Shut(stream)),
        })
    }
}

/// A socket, shut with its link (the reader's clone wakes up).
struct Shut(UnixStream);

impl Drop for Shut {
    fn drop(&mut self) {
        let _ = self.0.shutdown(std::net::Shutdown::Both);
    }
}

/// A child process, ended with its link.
struct Ended(Child);

impl Drop for Ended {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Bytes arriving in chunks (from an async task), read as a stream.
#[cfg(feature = "ssh")]
pub(crate) struct ChunkReader {
    chunks: mpsc::Receiver<Vec<u8>>,
    chunk: Vec<u8>,
    at: usize,
}

#[cfg(feature = "ssh")]
impl ChunkReader {
    pub(crate) fn new(chunks: mpsc::Receiver<Vec<u8>>) -> Self {
        Self { chunks, chunk: Vec::new(), at: 0 }
    }
}

#[cfg(feature = "ssh")]
impl Read for ChunkReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.at == self.chunk.len() {
            match self.chunks.recv() {
                Ok(chunk) => (self.chunk, self.at) = (chunk, 0),
                // The other end went: the end of the stream.
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.chunk.len() - self.at);
        buf[..n].copy_from_slice(&self.chunk[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// Bytes written as a stream, passed on in chunks (to an async task).
#[cfg(feature = "ssh")]
pub(crate) struct ChunkWriter(pub(crate) tokio::sync::mpsc::UnboundedSender<Vec<u8>>);

#[cfg(feature = "ssh")]
impl Write for ChunkWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.send(buf.to_vec()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "connection closed")
        })?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(all(test, feature = "ssh"))]
mod tests {
    use super::*;

    #[test]
    fn chunks_read_as_a_stream() {
        let (tx, rx) = mpsc::channel();
        tx.send(b"hel".to_vec()).unwrap();
        tx.send(Vec::new()).unwrap();
        tx.send(b"lo".to_vec()).unwrap();
        drop(tx);
        let mut text = String::new();
        ChunkReader::new(rx).read_to_string(&mut text).unwrap();
        assert_eq!(text, "hello");
    }
}
