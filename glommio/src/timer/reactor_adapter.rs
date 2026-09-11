// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience
//
//! The reactor's view of the timer wheel.
//!
//! Thin on purpose. The wheel owns its entries and knows where each one sits,
//! so this layer holds no index of its own — an earlier version kept a
//! parallel `HashMap` from timer to deadline and answered "when is the next
//! timer" by scanning it, which made every poll cost the whole population.

use super::{slab::TimerId, timing_wheel::TimingWheel};
use std::{
    task::Waker,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub(crate) struct ReactorTimers {
    wheel: TimingWheel,

    /// Kept across polls so that draining a batch of expiries allocates
    /// nothing. Always empty between calls.
    expired: Vec<(TimerId, Waker)>,
}

impl ReactorTimers {
    pub(crate) fn new() -> Self {
        Self {
            wheel: TimingWheel::new(),
            expired: Vec::new(),
        }
    }

    /// Register a timer, returning a handle that stays valid however many
    /// times the timer cascades.
    pub(crate) fn insert(&mut self, expires_at: Instant, waker: Waker) -> TimerId {
        self.wheel.insert(expires_at, waker)
    }

    /// Withdraw a timer, handing back its waker. `None` if it has already
    /// fired.
    pub(crate) fn remove(&mut self, id: TimerId) -> Option<Waker> {
        self.wheel.remove(id)
    }

    /// Whether a handle still names a live timer.
    pub(crate) fn contains(&self, id: TimerId) -> bool {
        self.wheel.contains(id)
    }

    /// Expire what is due, hand back the wakers, and say when to wake next.
    ///
    /// Wakers are returned rather than woken here. The caller holds a
    /// `RefMut` on the reactor's timers for the duration of this call, and
    /// waking under it would let a waker that touches a timer re-enter and
    /// panic on the second borrow.
    pub(crate) fn process_timers(&mut self, wakers: &mut Vec<Waker>) -> Option<Duration> {
        let now = Instant::now();
        self.wheel.advance_to(now);
        self.wheel.drain_expired_into(&mut self.expired);
        wakers.extend(self.expired.drain(..).map(|(_, waker)| waker));

        self.wheel
            .next_expiry()
            .map(|expires_at| expires_at.saturating_duration_since(now))
    }

    #[allow(dead_code)]
    pub(crate) fn len(&self) -> usize {
        self.wheel.len()
    }

    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        self.wheel.is_empty()
    }
}

impl Default for ReactorTimers {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: a waker that does nothing when woken.
    fn dummy_waker() -> Waker {
        Waker::noop().clone()
    }

    #[test]
    fn test_insert_and_process() {
        let mut timers = ReactorTimers::new();
        let now = Instant::now();

        // Insert a timer and get ID
        let _id = timers.insert(now + Duration::from_millis(100), dummy_waker());
        assert_eq!(timers.len(), 1);

        // Process before expiry - should not wake
        let mut wakers = Vec::new();
        let next = timers.process_timers(&mut wakers);
        assert_eq!(wakers.len(), 0);
        assert!(next.is_some());
        assert_eq!(timers.len(), 1);

        // Wait and process after expiry
        std::thread::sleep(Duration::from_millis(150));
        wakers.clear();
        timers.process_timers(&mut wakers);
        assert_eq!(wakers.len(), 1);
        assert_eq!(timers.len(), 0);
    }

    #[test]
    fn test_remove() {
        let mut timers = ReactorTimers::new();
        let now = Instant::now();

        // Insert and get ID
        let id = timers.insert(now + Duration::from_millis(100), dummy_waker());
        assert_eq!(timers.len(), 1);

        // Remove the timer using ID
        assert!(timers.remove(id).is_some());
        assert_eq!(timers.len(), 0);

        // Try to remove again with same ID - should return false
        assert!(timers.remove(id).is_none());
    }

    #[test]
    fn test_removing_twice_reports_the_second_as_absent() {
        let mut timers = ReactorTimers::new();
        let now = Instant::now();

        let id = timers.insert(now + Duration::from_millis(100), dummy_waker());
        assert_eq!(timers.len(), 1);

        assert!(
            timers.remove(id).is_some(),
            "the first removal withdraws it"
        );
        assert_eq!(timers.len(), 0);
        assert!(
            timers.remove(id).is_none(),
            "the second finds nothing to withdraw"
        );
    }

    #[test]
    fn test_multiple_timers() {
        let mut timers = ReactorTimers::new();
        let now = Instant::now();

        // Insert multiple timers
        let id1 = timers.insert(now + Duration::from_millis(100), dummy_waker());
        let id2 = timers.insert(now + Duration::from_millis(200), dummy_waker());
        let id3 = timers.insert(now + Duration::from_millis(300), dummy_waker());

        assert_eq!(timers.len(), 3);

        // Remove one timer
        assert!(timers.remove(id2).is_some());
        assert_eq!(timers.len(), 2);

        // The two that were not withdrawn are still withdrawable; the one
        // that was is not.
        assert!(timers.remove(id2).is_none(), "already gone");
        assert!(timers.remove(id1).is_some());
        assert!(timers.remove(id3).is_some());
        assert_eq!(timers.len(), 0);
    }
}
