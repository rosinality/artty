//! A small logger: messages go to stderr with a timestamp. For the server
//! that's its log file (`~/Library/Logs/artty/server.log`). Warnings and
//! errors by default; `ARTTY_LOG=info|debug|trace` for more.

use std::io::Write;

struct Stderr;

impl log::Log for Stderr {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let line = format!("{secs} {} [{}] {}\n", record.level(), record.target(), record.args());
        let _ = std::io::stderr().lock().write_all(line.as_bytes());
    }

    fn flush(&self) {}
}

/// Install the logger (once; later calls do nothing).
pub fn init() {
    let level = match std::env::var("ARTTY_LOG").as_deref() {
        Ok("trace") => log::LevelFilter::Trace,
        Ok("debug") => log::LevelFilter::Debug,
        Ok("info") => log::LevelFilter::Info,
        Ok("off") => log::LevelFilter::Off,
        _ => log::LevelFilter::Warn,
    };
    if log::set_logger(&Stderr).is_ok() {
        log::set_max_level(level);
    }
}
