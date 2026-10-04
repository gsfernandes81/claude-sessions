//! Work done off the menu's thread, so the menu can show a spinner while it waits
//! (owner, 2026-10-04).
//!
//! **The menu keeps the terminal.** An action that hands the terminal to a child — attach,
//! resume, a new session, a shell — calls `Terminal::suspend` and `resume` from the worker,
//! and the worker's terminal is a proxy: each call is a message to the menu's thread, which
//! does the real suspend or resume and answers. While the child has the terminal the menu's
//! thread does nothing but wait for the resume, so nothing draws over the child and nothing
//! reads its keys.
//!
//! **A spinner turns on a fixed cadence and never skips a frame.** Each deadline is set from
//! the last one rather than from when the frame happened to be drawn, so frames are evenly
//! spaced; one drawn late is followed by the next frame, not by the frame it would have been.
//! The owner asked for this on seeing a mockup step unevenly (2026-10-04).

use crate::ui::{Outcome, Terminal};
use std::io;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// How long work runs before its spinner shows: quick actions never flash one.
pub const SHOW_AFTER: Duration = Duration::from_millis(250);
/// One spinner frame: a little faster than Docker Compose's 100 ms (owner, 2026-10-04).
pub const FRAME: Duration = Duration::from_millis(80);

/// What the worker tells the menu.
#[derive(Debug)]
pub enum Event {
    /// Hand the terminal over to a child; answer with `Work::ack` once done.
    Suspend,
    /// Take the terminal back; answer with `Work::ack`.
    Resume,
    Done(Outcome),
}

pub type Job = Box<dyn FnOnce(&mut dyn Terminal) -> Outcome + Send>;

/// Work running on its own thread.
pub struct Work {
    events: Receiver<Event>,
    acks: Sender<()>,
}

/// The worker's terminal: each call is a request to the menu's thread.
struct Proxy {
    events: Sender<Event>,
    acks: Receiver<()>,
}

impl Proxy {
    fn ask(&self, ev: Event) -> io::Result<()> {
        let gone = || io::Error::new(io::ErrorKind::BrokenPipe, "the menu has gone");
        self.events.send(ev).map_err(|_| gone())?;
        self.acks.recv().map_err(|_| gone())
    }
}

impl Terminal for Proxy {
    fn suspend(&mut self) -> io::Result<()> {
        self.ask(Event::Suspend)
    }
    fn resume(&mut self) -> io::Result<()> {
        self.ask(Event::Resume)
    }
}

impl Work {
    pub fn spawn(job: Job) -> Work {
        let (ev_tx, events) = mpsc::channel();
        let (acks, ack_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut proxy = Proxy {
                events: ev_tx.clone(),
                acks: ack_rx,
            };
            let outcome = job(&mut proxy);
            let _ = ev_tx.send(Event::Done(outcome));
        });
        Work { events, acks }
    }

    /// The next event, waiting at most `timeout`. `None` when there is none yet.
    pub fn next(&self, timeout: Duration) -> Option<Event> {
        match self.events.recv_timeout(timeout) {
            Ok(ev) => Some(ev),
            Err(RecvTimeoutError::Timeout) => None,
            // A worker that panicked sends no Done; say so rather than wait for ever.
            Err(RecvTimeoutError::Disconnected) => Some(Event::Done(Outcome::Refused(
                "the action stopped without finishing".to_string(),
            ))),
        }
    }

    pub fn ack(&self) {
        let _ = self.acks.send(());
    }
}

/// A spinner's pacing: when it first shows, and when each frame after that is due.
#[derive(Debug, Clone)]
pub struct Spin {
    next: Instant,
    frame: Option<usize>,
}

impl Spin {
    /// A spinner that shows `delay` from `now`.
    pub fn after(now: Instant, delay: Duration) -> Spin {
        Spin {
            next: now + delay,
            frame: None,
        }
    }

    /// Advance if a frame is due at `now`. Returns the frame to draw when it changed. Steps by
    /// exactly one frame however late it is called, and keeps the cadence: the next deadline
    /// is one frame after the last deadline, or one frame from now if that has already passed.
    pub fn tick(&mut self, now: Instant) -> Option<usize> {
        if now < self.next {
            return None;
        }
        let frame = self.frame.map_or(0, |f| f + 1);
        self.frame = Some(frame);
        self.next += FRAME;
        if self.next <= now {
            self.next = now + FRAME;
        }
        Some(frame)
    }

    /// The frame showing, if the spinner has shown yet.
    pub fn frame(&self) -> Option<usize> {
        self.frame
    }

    /// How long until the next frame is due.
    pub fn until_next(&self, now: Instant) -> Duration {
        self.next.saturating_duration_since(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spinner_waits_then_steps_one_frame_at_a_time_on_a_fixed_cadence() {
        let t0 = Instant::now();
        let mut s = Spin::after(t0, SHOW_AFTER);
        assert_eq!(s.tick(t0), None, "not yet");
        assert_eq!(s.tick(t0 + SHOW_AFTER - Duration::from_millis(1)), None);
        assert_eq!(s.tick(t0 + SHOW_AFTER), Some(0), "shows at the delay");
        assert_eq!(s.tick(t0 + SHOW_AFTER + Duration::from_millis(10)), None);
        // On time: the next frame is due one frame after the last deadline.
        assert_eq!(s.until_next(t0 + SHOW_AFTER), FRAME);
        assert_eq!(s.tick(t0 + SHOW_AFTER + FRAME), Some(1));
        // A little late: still the next frame, and the cadence holds — the deadline after it
        // is measured from the deadline, not from when it was drawn.
        let late = t0 + SHOW_AFTER + 2 * FRAME + Duration::from_millis(15);
        assert_eq!(s.tick(late), Some(2), "never skips a frame");
        assert_eq!(s.until_next(late), FRAME - Duration::from_millis(15));
        // Very late — a stall of several frames — steps one frame, and restarts the cadence.
        let stalled = late + 10 * FRAME;
        assert_eq!(s.tick(stalled), Some(3), "one step, not seven");
        assert_eq!(s.until_next(stalled), FRAME);
    }

    #[test]
    fn work_hands_the_terminal_over_through_the_menu_and_reports_its_outcome() {
        let work = Work::spawn(Box::new(|t: &mut dyn Terminal| {
            t.suspend().unwrap();
            t.resume().unwrap();
            Outcome::Back(Some("done".into()))
        }));
        let wait = Duration::from_secs(5);
        assert!(matches!(work.next(wait), Some(Event::Suspend)));
        // The worker waits on the answer: nothing else arrives until the menu has suspended.
        assert!(work.next(Duration::from_millis(50)).is_none());
        work.ack();
        assert!(matches!(work.next(wait), Some(Event::Resume)));
        work.ack();
        match work.next(wait) {
            Some(Event::Done(Outcome::Back(Some(s)))) => assert_eq!(s, "done"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_worker_that_dies_is_reported_rather_waited_for() {
        let work = Work::spawn(Box::new(|_: &mut dyn Terminal| panic!("boom")));
        let mut got = None;
        for _ in 0..100 {
            if let Some(ev) = work.next(Duration::from_millis(50)) {
                got = Some(ev);
                break;
            }
        }
        assert!(
            matches!(got, Some(Event::Done(Outcome::Refused(_)))),
            "{got:?}"
        );
    }
}
