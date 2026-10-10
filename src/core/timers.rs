//! Timers for the background loops, shown on the status picture.
//!
//! A loop adds itself once with [`Timers::add`], then reports on the [`Timer`] it gets back:
//! [`Timer::running`] when it wakes up to do its work, and [`Timer::sleeping`] (or
//! [`Timer::next_at`]) before it goes back to sleep. The status picture lists every loop
//! that added itself, so a loop that never starts (its feature is turned off, say) isn't
//! shown.
//!
//! ```ignore
//! let timer = ctx.timers.add("Snail retries", "every 5m");
//! loop {
//!     timer.sleeping(CHECK_EVERY);
//!     tokio::time::sleep(CHECK_EVERY).await;
//!     timer.running();
//!     retry_due(&ctx).await;
//! }
//! ```

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};

/// Every loop's timer. Cheap to clone: the clones share one list.
#[derive(Clone, Default)]
pub struct Timers(Arc<Mutex<Vec<TimerState>>>);

/// What the status picture shows about one loop.
#[derive(Debug, Clone, PartialEq)]
pub struct TimerState {
    /// "Reminders"
    pub name: &'static str,
    /// How often it runs, like "every 5m" or "at the next reminder".
    pub every: &'static str,
    /// When it last woke up to do its work. `None` before the first time.
    pub last: Option<DateTime<Utc>>,
    /// When it wakes up next. `None` while it's working.
    pub next: Option<DateTime<Utc>>,
}

impl Timers {
    /// Adds a loop to the list. Adding a name that's already there (a loop started again)
    /// starts its timer over.
    pub fn add(&self, name: &'static str, every: &'static str) -> Timer {
        let mut list = self.0.lock().unwrap();
        let state = TimerState {
            name,
            every,
            last: None,
            next: None,
        };
        match list.iter_mut().find(|t| t.name == name) {
            Some(existing) => *existing = state,
            None => list.push(state),
        }
        Timer {
            timers: self.clone(),
            name,
        }
    }

    /// Every loop, in the order they were added.
    pub fn list(&self) -> Vec<TimerState> {
        self.0.lock().unwrap().clone()
    }

    fn update(&self, name: &str, change: impl FnOnce(&mut TimerState)) {
        if let Some(state) = self.0.lock().unwrap().iter_mut().find(|t| t.name == name) {
            change(state);
        }
    }
}

/// One loop's timer, from [`Timers::add`].
#[derive(Clone)]
pub struct Timer {
    timers: Timers,
    name: &'static str,
}

impl Timer {
    /// The loop woke up and is doing its work now.
    pub fn running(&self) {
        self.timers.update(self.name, |state| {
            state.last = Some(Utc::now());
            state.next = None;
        });
    }

    /// The loop goes to sleep for `wait`.
    pub fn sleeping(&self, wait: Duration) {
        let wait = TimeDelta::from_std(wait).unwrap_or(TimeDelta::MAX);
        self.next_at(
            Utc::now()
                .checked_add_signed(wait)
                .unwrap_or(DateTime::<Utc>::MAX_UTC),
        );
    }

    /// The loop goes to sleep until `at`.
    pub fn next_at(&self, at: DateTime<Utc>) {
        self.timers.update(self.name, |state| state.next = Some(at));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_runs_and_sleeps() {
        let timers = Timers::default();
        let timer = timers.add("Reminders", "at the next reminder");
        timers.add("Snail retries", "every 5m");
        assert_eq!(timers.list().len(), 2);
        assert_eq!(timers.list()[0].last, None);

        timer.running();
        let state = &timers.list()[0];
        assert!(state.last.is_some());
        assert_eq!(state.next, None);

        timer.sleeping(Duration::from_secs(60));
        let next = timers.list()[0].next.unwrap();
        let wait = next - Utc::now();
        assert!(wait > TimeDelta::seconds(55) && wait <= TimeDelta::seconds(60));

        // Started again: one row, from scratch.
        timers.add("Reminders", "at the next reminder");
        assert_eq!(timers.list().len(), 2);
        assert_eq!(timers.list()[0].next, None);
    }
}
