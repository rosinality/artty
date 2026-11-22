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
}

impl Default for Flow {
    fn default() -> Self {
        Self { in_flight: VecDeque::new(), window: START_WINDOW, delays: VecDeque::new() }
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
