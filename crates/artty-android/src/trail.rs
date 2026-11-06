//! The connection log: what logging in and connecting did, step by step,
//! for the app to show ("why won't it connect?"). artty's own messages
//! (info and up) and the libraries' warnings, the last few hundred kept
//! here, and each also written to Android's log (logcat, tag `artty`).

use std::collections::VecDeque;
use std::sync::Mutex;

/// Lines kept.
const KEPT: usize = 600;

static LINES: Mutex<VecDeque<(u64, log::Level, String)>> = Mutex::new(VecDeque::new());

struct Trail;

impl log::Log for Trail {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        let ours = metadata.target().starts_with("artty");
        metadata.level() <= if ours { log::Level::Info } else { log::Level::Warn }
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let text = record.args().to_string();
        logcat(record.level(), &text);
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let mut lines = LINES.lock().unwrap();
        if lines.len() == KEPT {
            lines.pop_front();
        }
        lines.push_back((millis, record.level(), text));
    }

    fn flush(&self) {}
}

/// Install it (once).
pub(crate) fn init() {
    if log::set_logger(&Trail).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
}

/// A line of the log.
#[derive(uniffi::Record)]
pub struct LogLine {
    /// Milliseconds since 1970.
    pub time: u64,
    /// `error`, `warn`, `info`.
    pub level: String,
    pub text: String,
}

/// The connection log, oldest first.
#[uniffi::export]
pub fn connection_log() -> Vec<LogLine> {
    LINES
        .lock()
        .unwrap()
        .iter()
        .map(|(time, level, text)| LogLine {
            time: *time,
            level: level.as_str().to_lowercase(),
            text: text.clone(),
        })
        .collect()
}

#[uniffi::export]
pub fn clear_connection_log() {
    LINES.lock().unwrap().clear();
}

#[cfg(target_os = "android")]
fn logcat(level: log::Level, text: &str) {
    use std::ffi::CString;
    #[link(name = "log")]
    unsafe extern "C" {
        fn __android_log_write(
            priority: i32,
            tag: *const std::ffi::c_char,
            text: *const std::ffi::c_char,
        ) -> i32;
    }
    // android/log.h's priorities.
    let priority = match level {
        log::Level::Error => 6,
        log::Level::Warn => 5,
        log::Level::Info => 4,
        _ => 3,
    };
    let (Ok(tag), Ok(text)) = (CString::new("artty"), CString::new(text.replace('\0', ""))) else {
        return;
    };
    // SAFETY: two NUL-terminated strings, alive for the call.
    unsafe { __android_log_write(priority, tag.as_ptr(), text.as_ptr()) };
}

#[cfg(not(target_os = "android"))]
fn logcat(_: log::Level, _: &str) {}
