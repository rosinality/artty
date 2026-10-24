//! Frame timing, printed to stderr when `ARTTY_PROFILE` is set.

use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

struct Stats {
    since: Instant,
    build: Vec<Duration>,
    paint: Vec<Duration>,
}

fn stats() -> Option<&'static Mutex<Stats>> {
    static STATS: OnceLock<Option<Mutex<Stats>>> = OnceLock::new();
    STATS
        .get_or_init(|| {
            std::env::var_os("ARTTY_PROFILE").map(|_| {
                Mutex::new(Stats { since: Instant::now(), build: Vec::new(), paint: Vec::new() })
            })
        })
        .as_ref()
}

pub fn enabled() -> bool {
    stats().is_some()
}

/// Record building a frame (`render`) and painting it.
pub fn record(build: Duration, paint: Duration) {
    let Some(stats) = stats() else { return };
    let mut stats = stats.lock().unwrap();
    stats.build.push(build);
    stats.paint.push(paint);
    if stats.since.elapsed() >= Duration::from_secs(2) {
        let report = |times: &mut Vec<Duration>| {
            times.sort();
            let ms = |d: Duration| d.as_secs_f64() * 1000.;
            let p95 = times[(times.len() * 95 / 100).min(times.len() - 1)];
            let mean = times.iter().sum::<Duration>() / times.len() as u32;
            format!("mean {:.2}ms p95 {:.2}ms", ms(mean), ms(p95))
        };
        let frames = stats.build.len();
        let seconds = stats.since.elapsed().as_secs_f64();
        let (build, paint) = (report(&mut stats.build), report(&mut stats.paint));
        eprintln!(
            "artty profile: {:.0} fps; build {build}; paint {paint}",
            frames as f64 / seconds
        );
        *stats = Stats { since: Instant::now(), build: Vec::new(), paint: Vec::new() };
    }
}
