//! What the main loop works through: apps' messages and `artty` requests
//! first, then panes' events, each pane's merged into one entry.
//!
//! A pane printing without pause produces events as fast as its reader
//! parses, and every app shares the one main loop with it. So its events
//! are merged as they come (new output, cursor and title changes into the
//! latest; bells into one) and wait at most once in the queue however fast
//! they come; what can't be merged waits in order, up to a limit. And they
//! come after anything from an app, so keys, pings and new windows never
//! wait behind output. Readers never wait for the main loop: they send
//! events holding their terminal's lock, which the main loop may need.

use std::collections::{HashMap, VecDeque};
use std::sync::{Condvar, Mutex};
use std::time::Instant;

use artty_core::layout::PaneId;
use artty_core::vt::event::Event;

use crate::Input;

/// Most events a pane keeps waiting that can't be merged (shell marks,
/// notifications, ...): more means something sends nothing else, and the
/// newest are dropped.
const MAX_WAITING: usize = 1024;

/// A pane's events waiting for the main loop.
#[derive(Default)]
pub(crate) struct PaneEvents {
    /// Those to handle one by one, in order.
    pub events: VecDeque<Event>,
    /// The latest title (`None`: back to the default), if it changed.
    pub title: Option<Option<String>>,
    pub bell: bool,
    /// Something on screen changed.
    pub redraw: bool,
    /// Events dropped over [`MAX_WAITING`].
    pub dropped: usize,
}

impl PaneEvents {
    /// Them as events, in an order to handle them in.
    pub fn into_events(self) -> impl Iterator<Item = Event> {
        let title = self.title.map(|title| match title {
            Some(title) => Event::Title(title),
            None => Event::ResetTitle,
        });
        self.events
            .into_iter()
            .chain(title)
            .chain(self.bell.then_some(Event::Bell))
            .chain(self.redraw.then_some(Event::Wakeup))
    }

    fn add(&mut self, event: Event) {
        match event {
            Event::Wakeup | Event::MouseCursorDirty | Event::CursorBlinkingChange => {
                self.redraw = true
            },
            Event::Title(title) => self.title = Some(Some(title)),
            Event::ResetTitle => self.title = Some(None),
            Event::Bell => self.bell = true,
            // The end of the pane: always kept.
            event @ (Event::Exit | Event::ChildExit(_)) => self.events.push_back(event),
            _ if self.events.len() >= MAX_WAITING => self.dropped += 1,
            event => self.events.push_back(event),
        }
    }
}

/// The next thing to handle.
pub(crate) enum Next {
    Input(Input),
    Pane(PaneId, PaneEvents),
}

#[derive(Default)]
struct Queues {
    inputs: VecDeque<Input>,
    /// Panes with events waiting, in the order their first came.
    panes: VecDeque<PaneId>,
    events: HashMap<PaneId, PaneEvents>,
    /// The main loop is gone: senders stop.
    closed: bool,
}

#[derive(Default)]
pub(crate) struct Inbox {
    queues: Mutex<Queues>,
    arrived: Condvar,
}

/// The main loop is gone.
#[derive(Debug)]
pub(crate) struct Closed;

impl Inbox {
    /// From an app, an `artty` request, a connection: ahead of any pane's.
    pub fn send(&self, input: Input) -> Result<(), Closed> {
        let mut queues = self.lock();
        if queues.closed {
            return Err(Closed);
        }
        queues.inputs.push_back(input);
        drop(queues);
        self.arrived.notify_one();
        Ok(())
    }

    /// From a pane's reader.
    pub fn pane_event(&self, pane: PaneId, event: Event) {
        let mut queues = self.lock();
        let queues = &mut *queues;
        let waiting = queues.events.entry(pane).or_insert_with(|| {
            queues.panes.push_back(pane);
            PaneEvents::default()
        });
        let first = waiting.events.is_empty()
            && waiting.title.is_none()
            && !waiting.bell
            && !waiting.redraw;
        waiting.add(event);
        if first {
            self.arrived.notify_one();
        }
    }

    /// Wait until there's something, or until `deadline`.
    pub fn wait(&self, deadline: Instant) {
        let mut queues = self.lock();
        while queues.inputs.is_empty() && queues.panes.is_empty() {
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            queues = match self.arrived.wait_timeout(queues, deadline - now) {
                Ok((queues, _)) => queues,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    /// The next thing to handle, if any: inputs before panes.
    pub fn next(&self) -> Option<Next> {
        let mut queues = self.lock();
        if let Some(input) = queues.inputs.pop_front() {
            return Some(Next::Input(input));
        }
        let pane = queues.panes.pop_front()?;
        let events = queues.events.remove(&pane).unwrap_or_default();
        Some(Next::Pane(pane, events))
    }

    /// The main loop is stopping: nothing more is taken.
    pub fn close(&self) {
        let mut queues = self.lock();
        queues.closed = true;
        queues.inputs.clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Queues> {
        // Nothing here panics while holding it; and if something did, the
        // queues are still whole.
        self.queues.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Input;

    fn events(next: Option<Next>) -> (PaneId, Vec<String>) {
        let Some(Next::Pane(pane, events)) = next else { panic!("not a pane's events") };
        (pane, events.into_events().map(|e| format!("{e:?}")).collect())
    }

    #[test]
    fn a_pane_waits_once_however_much_it_prints() {
        let inbox = Inbox::default();
        for _ in 0..10_000 {
            inbox.pane_event(1, Event::Wakeup);
            inbox.pane_event(1, Event::Title("a".into()));
        }
        inbox.pane_event(2, Event::Wakeup);
        inbox.pane_event(1, Event::Title("b".into()));
        inbox.pane_event(1, Event::Bell);
        inbox.pane_event(1, Event::Bell);
        let (pane, got) = events(inbox.next());
        assert_eq!(pane, 1);
        assert_eq!(got, ["Title(b)", "Bell", "Wakeup"]);
        assert_eq!(events(inbox.next()), (2, vec!["Wakeup".to_owned()]));
        assert!(inbox.next().is_none());

        // Output after it was taken waits again.
        inbox.pane_event(1, Event::Wakeup);
        assert_eq!(events(inbox.next()).0, 1);
    }

    #[test]
    fn apps_come_first() {
        let inbox = Inbox::default();
        inbox.pane_event(1, Event::Wakeup);
        inbox.send(Input::Disconnected(7)).unwrap();
        assert!(matches!(inbox.next(), Some(Next::Input(Input::Disconnected(7)))));
        assert!(matches!(inbox.next(), Some(Next::Pane(1, _))));
    }

    #[test]
    fn what_cant_be_merged_waits_in_order_up_to_a_limit() {
        let inbox = Inbox::default();
        let mark = |n| Event::Notification { title: None, body: format!("{n}") };
        for n in 0..MAX_WAITING + 10 {
            inbox.pane_event(1, mark(n));
        }
        inbox.pane_event(1, Event::Exit);
        let Some(Next::Pane(_, waiting)) = inbox.next() else { panic!() };
        assert_eq!(waiting.dropped, 10);
        assert_eq!(waiting.events.len(), MAX_WAITING + 1);
        assert!(
            matches!(waiting.events.front(), Some(Event::Notification { body, .. }) if body == "0")
        );
        assert!(matches!(waiting.events.back(), Some(Event::Exit)));
    }

    #[test]
    fn closed_takes_nothing_more() {
        let inbox = Inbox::default();
        inbox.close();
        assert!(inbox.send(Input::Disconnected(1)).is_err());
    }
}
