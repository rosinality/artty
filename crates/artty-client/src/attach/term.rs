//! This terminal, taken over while attached: raw mode, the alternate
//! screen, the mouse, focus and pastes reported; all of it put back on
//! leaving, a panic's too. Its size, and the signals that matter (a new
//! size, being told to end).

use std::io::{self, Read, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, Ordering};

/// The alternate screen, no wrapping at the edge, the mouse (buttons and
/// drags, SGR), focus and bracketed pastes reported, the screen cleared.
const ENTER: &str =
    "\x1b[?1049h\x1b[?7l\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?1004h\x1b[?2004h\x1b[0m\x1b[2J";
/// As it was: the cursor's shape and showing too.
const LEAVE: &str = "\x1b[?2004l\x1b[?1004l\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?7h\x1b[0m\x1b[0 q\x1b[?25h\x1b[?1049l";

/// The terminal's settings before, while taken over.
static SAVED: Mutex<Option<libc::termios>> = Mutex::new(None);

/// This terminal taken over, until dropped.
pub struct Terminal(());

impl Terminal {
    pub fn enter() -> Result<Terminal, String> {
        // SAFETY: termios calls on stdin with valid pointers.
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut saved) != 0 {
                return Err("not in a terminal".to_owned());
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
                return Err(io::Error::last_os_error().to_string());
            }
            *SAVED.lock().unwrap() = Some(saved);
        }
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            hook(info);
        }));
        write_out(ENTER);
        Ok(Terminal(()))
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        restore();
    }
}

/// The terminal as it was (once: later calls do nothing).
fn restore() {
    let Some(saved) = SAVED.lock().map_or(None, |mut saved| saved.take()) else { return };
    write_out(LEAVE);
    // SAFETY: restoring what was read.
    unsafe { libc::tcsetattr(0, libc::TCSANOW, &saved) };
}

/// Written whole, past interruptions; a terminal gone is no matter.
pub fn write_out(text: &str) {
    let mut out = io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

/// The terminal's size: columns, lines, and a cell's pixels (or a guess).
pub fn size() -> (usize, usize, u16, u16) {
    // SAFETY: plain ioctl into a zeroed winsize.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let ok =
        [1, 0].into_iter().any(|fd| unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0);
    let (columns, lines) =
        if ok && size.ws_col > 0 { (size.ws_col, size.ws_row) } else { (80, 24) };
    let cell = |pixels: u16, cells: u16, guess: u16| {
        if pixels > 0 { (pixels / cells.max(1)).max(1) } else { guess }
    };
    (
        usize::from(columns),
        usize::from(lines.max(2)),
        cell(size.ws_xpixel, columns, 8),
        cell(size.ws_ypixel, lines, 16),
    )
}

/// What's typed, as it comes, to `send` (until it says stop, or input ends).
pub fn read_input(send: impl Fn(Vec<u8>) -> bool + Send + 'static) {
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => return,
                Ok(n) if !send(buf[..n].to_vec()) => return,
                Ok(_) => {},
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {},
                Err(_) => return,
            }
        }
    });
}

/// The pipe a signal is told down (its handler may do little else).
static SIGNALS: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(signal: libc::c_int) {
    let fd = SIGNALS.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = signal as u8;
        // SAFETY: write is async-signal-safe; one byte from the stack.
        unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) };
    }
}

/// A new size (SIGWINCH), and being told to end (SIGTERM, SIGHUP), to
/// `send` as they come.
pub fn watch_signals(send: impl Fn(libc::c_int) -> bool + Send + 'static) -> Result<(), String> {
    let mut fds = [0; 2];
    // SAFETY: pipe into a two-int array.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    SIGNALS.store(fds[1], Ordering::Relaxed);
    for signal in [libc::SIGWINCH, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: a handler that only writes to the pipe.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as *const () as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
    let read_fd = fds[0];
    std::thread::spawn(move || {
        let mut byte = 0u8;
        loop {
            // SAFETY: a byte into a byte.
            let n = unsafe { libc::read(read_fd, (&mut byte as *mut u8).cast(), 1) };
            if n == 1 {
                if !send(libc::c_int::from(byte)) {
                    return;
                }
            } else if n == 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return;
            }
        }
    });
    Ok(())
}
