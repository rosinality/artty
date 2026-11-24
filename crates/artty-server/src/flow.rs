//! How many screen updates may be on their way to an app before it says it
//! has them: a window of bytes, kept as large as its link takes without
//! the updates queueing on it.
//!
//! Each batch of updates ends with a `Sync` the app acknowledges. The time
//! that takes is the round trip plus however long the batch waited in
//! queues; the least of it seen lately stands for the round trip alone. While
//! batches come back within [`TARGET`] of it, the window grows; when they're
//! later, updates are queueing, and it shrinks (as LEDBAT does). One batch may
//! always be on its way, however large.
//!
//! A stall is not queueing: this machine, or one in between, stops for
//! seconds (out of memory, say), then everything comes back at once. Each of
//! those acknowledgments is late by the stall, and counting them would close
//! the window just as the link is back. A queue never silences the app,
//! whose pings come up the other way, unqueued; a stall does. So hearing
//! nothing at all for a while (longer than between its pings) marks one, and
//! batches sent before it don't count either way.
//!
//! On a fast link the window stays wide open. On a slow one it keeps the
//! queue short, so that when updates wait, they wait here, where a newer
//! screen replaces them, and not on the link, where everything behind them
//! (what was typed coming back) waits too.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Delay over the round trip that counts as queueing.
const TARGET: Duration = Duration::from_millis(30);
/// The round trip is the least delay in this bucket and the one before it
/// (as LEDBAT keeps it): a busy link's longer delays never stand for it
/// for less than this long.
const BUCKET: Duration = Duration::from_secs(30);
/// Silence over this many of the app's ping intervals, with batches on
/// their way: a stall. Its interval unknown yet, over `STALL_UNKNOWN`.
const STALL_PINGS: f32 = 1.5;
const STALL_UNKNOWN: Duration = Duration::from_millis(7500);
const MIN_WINDOW: usize = 4 * 1024;
const MAX_WINDOW: usize = 4 * 1024 * 1024;
const START_WINDOW: usize = 64 * 1024;
/// How far past the window updates for what was just typed may go.
const TYPED_ALLOWANCE: usize = 32 * 1024;

#[derive(Debug)]
pub(crate) struct Flow {
    /// Batches not yet acknowledged: sequence number, bytes, when sent.
    in_flight: VecDeque<(u64, usize, Instant)>,
    window: usize,
    /// The least acknowledgment delay in each of the last two buckets
    /// (when the bucket began, its least).
    delays: VecDeque<(Instant, Duration)>,
    /// When the app was last heard from, and last pinged.
    last_heard: Option<Instant>,
    last_ping: Option<Instant>,
    /// The app's ping interval, as seen lately (the least of the last few).
    ping_every: VecDeque<Duration>,
    /// When the latest stall ended: batches sent before then say nothing
    /// of the link.
    stalled_until: Option<Instant>,
}

impl Default for Flow {
    fn default() -> Self {
        Self {
            in_flight: VecDeque::new(),
            window: START_WINDOW,
            delays: VecDeque::new(),
            last_heard: None,
            last_ping: None,
            ping_every: VecDeque::new(),
            stalled_until: None,
        }
    }
}

impl Flow {
    /// Bytes sent and not yet acknowledged.
    pub(crate) fn in_flight(&self) -> usize {
        self.in_flight.iter().map(|(_, bytes, _)| bytes).sum()
    }

    /// Whether another batch may go now.
    pub(crate) fn open(&self) -> bool {
        self.in_flight.is_empty() || self.in_flight() < self.window
    }

    /// Whether a batch for what was just typed may go now: a little past
    /// the window, so it doesn't wait behind others, but not without bound
    /// (a program printing all it can in the pane typed into).
    pub(crate) fn open_for_typed(&self) -> bool {
        self.in_flight.is_empty() || self.in_flight() < self.window + TYPED_ALLOWANCE
    }

    pub(crate) fn sent(&mut self, seq: u64, bytes: usize, now: Instant) {
        self.in_flight.push_back((seq, bytes, now));
    }

    /// The app has every batch up to `seq`.
    /// Anything from the app (`ping`: a ping). After a silence longer
    /// than its pings leave, with batches on their way: a stall ends now.
    pub(crate) fn heard(&mut self, ping: bool, now: Instant) {
        let stall = match self.ping_every.iter().min() {
            Some(every) => every.mul_f32(STALL_PINGS),
            None => STALL_UNKNOWN,
        };
        if let Some(last) = self.last_heard
            && now.saturating_duration_since(last) > stall
            && !self.in_flight.is_empty()
        {
            self.stalled_until = Some(now);
        }
        self.last_heard = Some(now);
        if ping {
            // (Not across a stall: that's no interval of its.)
            if let Some(last) = self.last_ping
                && self.stalled_until != Some(now)
            {
                self.ping_every.push_back(now.saturating_duration_since(last));
                if self.ping_every.len() > 4 {
                    self.ping_every.pop_front();
                }
            }
            self.last_ping = Some(now);
        }
    }

    pub(crate) fn acked(&mut self, seq: u64, now: Instant) {
        let mut newest = None;
        let mut bytes = 0;
        while let Some(&(sent_seq, sent_bytes, at)) = self.in_flight.front() {
            if sent_seq > seq {
                break;
            }
            self.in_flight.pop_front();
            bytes += sent_bytes;
            newest = Some(at);
        }
        let Some(sent) = newest else { return };
        if self.stalled_until.is_some_and(|until| sent < until) {
            return;
        }
        let delay = now.saturating_duration_since(sent);
        match self.delays.back_mut() {
            Some((began, least)) if now.saturating_duration_since(*began) < BUCKET => {
                *least = (*least).min(delay);
            },
            _ => {
                self.delays.push_back((now, delay));
                if self.delays.len() > 2 {
                    self.delays.pop_front();
                }
            },
        }
        let round_trip = self.delays.iter().map(|(_, least)| *least).min().unwrap_or(delay);
        if delay.saturating_sub(round_trip) <= TARGET {
            self.window = (self.window + bytes).min(MAX_WINDOW);
        } else {
            self.window = (self.window * 3 / 4).max(MIN_WINDOW);
        }
    }

    #[cfg(test)]
    fn window(&self) -> usize {
        self.window
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn one_batch_always_goes() {
        let mut flow = Flow::default();
        let t = Instant::now();
        assert!(flow.open());
        // A batch larger than any window: it goes, and then nothing more.
        flow.sent(1, MAX_WINDOW * 2, t);
        assert!(!flow.open());
        flow.acked(1, t + ms(5));
        assert!(flow.open());
        assert_eq!(flow.in_flight(), 0);
    }

    #[test]
    fn small_updates_never_wait_for_acknowledgments() {
        // Lines scrolling on a 100 ms round trip: many small batches on
        // their way at once, none waiting.
        let mut flow = Flow::default();
        let t = Instant::now();
        for seq in 1..=12 {
            assert!(flow.open(), "batch {seq}");
            flow.sent(seq, 300, t + ms(8 * seq));
        }
        flow.acked(12, t + ms(200));
        assert!(flow.open());
    }

    #[test]
    fn a_queueing_link_closes_the_window() {
        // 300 ms round trip; 16 KB batches take 500 ms each at 256 kbit/s.
        let mut flow = Flow::default();
        let mut t = Instant::now();
        // A small one first: the round trip.
        flow.sent(1, 100, t);
        flow.acked(1, t + ms(300));
        let mut seq = 1;
        for _ in 0..20 {
            seq += 1;
            flow.sent(seq, 16 * 1024, t);
            t += ms(800);
            flow.acked(seq, t);
        }
        assert_eq!(flow.window(), MIN_WINDOW);
        // So a second flood batch waits for the first.
        flow.sent(seq + 1, 16 * 1024, t);
        assert!(!flow.open());
    }

    /// The far end stops for seconds with batches on their way, then
    /// answers them all at once: the window stays as it was.
    #[test]
    fn a_stall_leaves_the_window_open() {
        let mut flow = Flow::default();
        let mut t = Instant::now();
        // A 100 ms link, open well past where it started.
        for seq in 1..=20 {
            flow.sent(seq, 16 * 1024, t);
            t += ms(100);
            flow.acked(seq, t);
        }
        let window = flow.window();
        assert!(window > START_WINDOW, "{window}");
        // The app pings every second.
        for _ in 0..3 {
            flow.heard(true, t);
            t += ms(1000);
        }
        // Eight batches go; nothing comes back for 5 s; then everything.
        for seq in 21..=28 {
            flow.sent(seq, 8 * 1024, t + ms(seq - 20));
        }
        t += ms(5000);
        flow.heard(true, t);
        for seq in 21..=28 {
            flow.heard(false, t + ms(seq - 20));
            flow.acked(seq, t + ms(seq - 20));
        }
        assert!(flow.window() >= window, "{} < {window}", flow.window());
        // And the link after it counts again, as before.
        t += ms(50);
        flow.sent(29, 16 * 1024, t);
        flow.heard(false, t + ms(100));
        flow.acked(29, t + ms(100));
        assert!(flow.window() > window);
        flow.sent(30, 16 * 1024, t);
        flow.heard(true, t + ms(900));
        flow.acked(30, t + ms(900));
        assert!(flow.window() < window + 32 * 1024, "late again: queueing");
    }

    /// A link so slow each batch takes 2 s: answers far apart, but the
    /// app's pings keep coming. A queue, not a stall: the window closes.
    #[test]
    fn a_very_slow_link_is_no_stall() {
        let mut flow = Flow::default();
        let mut t = Instant::now();
        flow.sent(1, 100, t);
        flow.heard(true, t + ms(300));
        flow.acked(1, t + ms(300));
        let mut seq = 1;
        for _ in 0..20 {
            seq += 1;
            flow.sent(seq, 16 * 1024, t);
            for _ in 0..2 {
                t += ms(1000);
                flow.heard(true, t);
            }
            flow.acked(seq, t);
        }
        assert_eq!(flow.window(), MIN_WINDOW);
    }

    #[test]
    fn a_fast_link_opens_it() {
        let mut flow = Flow::default();
        let t = Instant::now();
        for seq in 1..=200 {
            flow.sent(seq, 64 * 1024, t + ms(seq));
            flow.acked(seq, t + ms(seq + 1));
        }
        assert_eq!(flow.window(), MAX_WINDOW);
    }

    #[test]
    fn stale_or_unknown_acknowledgments_change_nothing() {
        let mut flow = Flow::default();
        let t = Instant::now();
        // What was typed goes a little past the window, not without bound.
        flow.sent(1, START_WINDOW, t);
        assert!(!flow.open() && flow.open_for_typed());
        flow.sent(2, TYPED_ALLOWANCE, t);
        assert!(!flow.open_for_typed());
        flow.acked(2, t + ms(5));
        flow.sent(5, 1000, t);
        flow.acked(3, t + ms(10));
        assert_eq!(flow.in_flight(), 1000);
        flow.acked(9, t + ms(20));
        assert_eq!(flow.in_flight(), 0);
        let window = flow.window();
        flow.acked(9, t + ms(30));
        assert_eq!(flow.window(), window);
    }
}
