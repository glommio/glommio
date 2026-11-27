//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! The reactor's view of the timer wheel.
//!
//! Thin on purpose. The wheel owns its entries and knows where each one sits,
//! so this layer holds no index of its own. An earlier version kept a
//! parallel `HashMap` from timer to deadline and answered "when is the next
//! timer" by scanning it, which made every poll cost the whole population.
//!
//! Timers registered from a `Latency::Matters` queue live in a second wheel,
//! so that the earliest of them is found as cheaply as the earliest of all.

use super::wheel::{Key, Millis, TimingWheel};
use std::{task::Waker, time::Instant};

/// A registered timer: its handle in the wheel that holds it, and which
/// wheel that is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TimerId {
    key: Key,
    latency: bool,
}

type Wheel = TimingWheel<Waker, Millis>;

#[derive(Debug)]
pub(crate) struct ReactorTimers {
    wheel: Wheel,

    /// The timers registered from `Latency::Matters` queues. Built on the
    /// first such timer: an executor without one does not pay for a second
    /// wheel's slots.
    latency: Option<Box<Wheel>>,

    /// No later than the earliest timer in [`Self::latency`], and `None` when
    /// it holds none. Advancing a wheel and asking it for its earliest entry
    /// each cost hundreds of instructions, which the reactor would pay on
    /// every pass, so the latency wheel is advanced and asked only once this
    /// is due. A timer withdrawn before it is due leaves the bound early,
    /// which costs at most a pass that finds nothing to do.
    latency_due: Option<Instant>,

    /// Kept across polls so that draining a batch of expiries allocates
    /// nothing. Always empty between calls.
    expired: Vec<(Key, Waker)>,
}

impl ReactorTimers {
    pub(crate) fn new() -> Self {
        Self {
            wheel: TimingWheel::new(),
            latency: None,
            latency_due: None,
            expired: Vec::new(),
        }
    }

    /// The wheel that holds `id`, if it has been built.
    fn wheel_of(&mut self, id: TimerId) -> Option<&mut Wheel> {
        if id.latency {
            self.latency.as_deref_mut()
        } else {
            Some(&mut self.wheel)
        }
    }

    /// Register a timer, returning a handle that stays valid however many
    /// times the timer cascades. `latency` says the timer was registered from
    /// a `Latency::Matters` queue.
    pub(crate) fn insert(&mut self, expires_at: Instant, waker: Waker, latency: bool) -> TimerId {
        let wheel = if latency {
            self.latency_due = Some(
                self.latency_due
                    .map_or(expires_at, |due| due.min(expires_at)),
            );
            self.latency.get_or_insert_with(Box::default)
        } else {
            &mut self.wheel
        };
        TimerId {
            key: wheel.insert(expires_at, waker),
            latency,
        }
    }

    /// Withdraw a timer, handing back its waker. `None` if it has already
    /// fired.
    pub(crate) fn remove(&mut self, id: TimerId) -> Option<Waker> {
        let wheel = self.wheel_of(id)?;
        let removed = wheel.remove(id.key);
        if id.latency && wheel.is_empty() {
            self.latency_due = None;
        }
        removed
    }

    /// Points a registered timer at `waker`.
    ///
    /// `None` when the handle no longer names a timer. Otherwise the waker it
    /// displaced, if any: none when the timer already wakes the same task,
    /// which is a re-poll from it and costs neither a clone nor a drop. The
    /// displaced waker is handed back rather than dropped here, since dropping
    /// a task's last waker can run code that reaches for these timers.
    pub(crate) fn update_waker(&mut self, id: TimerId, waker: &Waker) -> Option<Option<Waker>> {
        let current = self.wheel_of(id)?.get_mut(id.key)?;
        if current.will_wake(waker) {
            return Some(None);
        }
        Some(Some(std::mem::replace(current, waker.clone())))
    }

    /// Whether a handle still names a live timer.
    pub(crate) fn contains(&self, id: TimerId) -> bool {
        if id.latency {
            self.latency.as_ref().is_some_and(|w| w.contains(id.key))
        } else {
            self.wheel.contains(id.key)
        }
    }

    /// Expire what is due, hand back the wakers, and say when to wake next.
    ///
    /// Wakers are returned rather than woken here. The caller holds a
    /// `RefMut` on the reactor's timers for the duration of this call, and
    /// waking under it would let a waker that touches a timer re-enter and
    /// panic on the second borrow.
    ///
    /// The latency timers are drained first, so that of the queues woken
    /// together, a latency queue enters the run queue first.
    pub(crate) fn process_timers(&mut self, wakers: &mut Vec<Waker>) {
        let now = Instant::now();
        if let Some(latency) = self.latency.as_deref_mut() {
            if self.latency_due.is_some_and(|due| due <= now) {
                Self::expire(latency, now, &mut self.expired, wakers);
                self.latency_due = latency.next_expiry();
            }
        }
        Self::expire(&mut self.wheel, now, &mut self.expired, wakers);
    }

    /// Advances `wheel` to `now` and hands back the wakers of what came due.
    fn expire(
        wheel: &mut Wheel,
        now: Instant,
        expired: &mut Vec<(Key, Waker)>,
        wakers: &mut Vec<Waker>,
    ) {
        wheel.advance_to(now);
        wheel.drain_expired_into(expired);
        if !expired.is_empty() {
            wakers.extend(expired.drain(..).map(|(_, waker)| waker));
        }
    }

    /// When the next armed timer is due, and when the next latency timer is,
    /// each `None` when there is none.
    ///
    /// Separate from [`Self::process_timers`] because the spinning poll path
    /// wakes what is due without caring when the next one is. Asked together
    /// so that a pass reads the latency wheel once.
    pub(crate) fn next_expiries(&self) -> (Option<Instant>, Option<Instant>) {
        let latency = self.next_latency_expiry();
        let next = match (self.wheel.next_expiry(), latency) {
            (Some(plain), Some(latency)) => Some(plain.min(latency)),
            (plain, latency) => plain.or(latency),
        };
        (next, latency)
    }

    /// The delay until the next armed timer, or `None` when none are.
    #[cfg(test)]
    fn next_timer(&self) -> Option<std::time::Duration> {
        self.next_expiries()
            .0
            .map(|expires_at| expires_at.saturating_duration_since(Instant::now()))
    }

    /// When the earliest timer registered from a `Latency::Matters` queue is
    /// due, or `None` when there is none.
    ///
    /// Never later than the truth, and early only by a timer withdrawn since.
    fn next_latency_expiry(&self) -> Option<Instant> {
        self.latency_due
    }

    #[allow(dead_code)]
    pub(crate) fn len(&self) -> usize {
        self.wheel.len() + self.latency.as_ref().map_or(0, |wheel| wheel.len())
    }

    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
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
    use std::time::Duration;

    /// A waker that does nothing when woken.
    fn dummy_waker() -> Waker {
        Waker::noop().clone()
    }

    #[test]
    fn test_insert_and_process() {
        let mut timers = ReactorTimers::new();
        let now = Instant::now();

        let _id = timers.insert(now + Duration::from_millis(100), dummy_waker(), false);
        assert_eq!(timers.len(), 1);

        let mut wakers = Vec::new();
        timers.process_timers(&mut wakers);
        assert_eq!(wakers.len(), 0);
        assert!(timers.next_timer().is_some());
        assert_eq!(timers.len(), 1);

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

        let id = timers.insert(now + Duration::from_millis(100), dummy_waker(), false);
        assert_eq!(timers.len(), 1);

        assert!(timers.remove(id).is_some());
        assert_eq!(timers.len(), 0);

        assert!(timers.remove(id).is_none());
    }

    #[test]
    fn test_removing_twice_reports_the_second_as_absent() {
        let mut timers = ReactorTimers::new();
        let now = Instant::now();

        let id = timers.insert(now + Duration::from_millis(100), dummy_waker(), false);
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

        let id1 = timers.insert(now + Duration::from_millis(100), dummy_waker(), false);
        let id2 = timers.insert(now + Duration::from_millis(200), dummy_waker(), false);
        let id3 = timers.insert(now + Duration::from_millis(300), dummy_waker(), false);

        assert_eq!(timers.len(), 3);

        assert!(timers.remove(id2).is_some());
        assert_eq!(timers.len(), 2);

        assert!(timers.remove(id2).is_none(), "already gone");
        assert!(timers.remove(id1).is_some());
        assert!(timers.remove(id3).is_some());
        assert_eq!(timers.len(), 0);
    }

    /// A waker that records `tag` in `log` when woken.
    fn logging_waker(log: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>, tag: u8) -> Waker {
        struct Logged(std::sync::Arc<std::sync::Mutex<Vec<u8>>>, u8);
        impl std::task::Wake for Logged {
            fn wake(self: std::sync::Arc<Self>) {
                self.0.lock().unwrap().push(self.1);
            }
        }
        Waker::from(std::sync::Arc::new(Logged(log.clone(), tag)))
    }

    /// Only latency timers count toward the next latency expiry, every timer
    /// counts toward the next timer, and a handle reaches its timer in
    /// whichever wheel holds it.
    #[test]
    fn latency_timers_are_kept_apart_and_reached_through_their_handles() {
        let mut timers = ReactorTimers::new();
        let now = Instant::now();
        let plain = timers.insert(now + Duration::from_millis(100), dummy_waker(), false);
        assert_eq!(timers.next_latency_expiry(), None);

        let later = now + Duration::from_millis(300);
        let latency = timers.insert(later, dummy_waker(), true);
        let expiry = timers
            .next_latency_expiry()
            .expect("a latency timer is armed");
        assert!(expiry <= later && expiry > now + Duration::from_millis(200));
        assert!(timers.next_timer().unwrap() <= Duration::from_millis(100));
        let last = timers.insert(now + Duration::from_millis(400), dummy_waker(), true);
        assert_eq!(
            timers.next_latency_expiry(),
            Some(expiry),
            "a later latency timer moved the earliest"
        );
        assert!(timers.remove(last).is_some());

        assert!(timers.contains(plain) && timers.contains(latency));
        assert!(matches!(
            timers.update_waker(latency, &dummy_waker()),
            Some(None)
        ));
        assert!(timers.remove(latency).is_some());
        assert!(!timers.contains(latency));
        assert!(timers.contains(plain), "the plain timer is untouched");
        assert_eq!(timers.next_latency_expiry(), None);
        assert!(timers.remove(plain).is_some());
        assert!(timers.is_empty());

        timers.insert(later, dummy_waker(), true);
        assert!(
            timers.next_timer().is_some(),
            "a park must wake for a latency timer too"
        );
    }

    /// The latency wheel is advanced only when its earliest timer is due, so
    /// a timer can be registered into it long after it was last advanced,
    /// and must still be woken when due and not before.
    #[test]
    fn a_latency_timer_registered_into_an_idle_wheel_fires_when_due() {
        let mut timers = ReactorTimers::new();
        let mut wakers = Vec::new();
        let first = timers.insert(Instant::now(), dummy_waker(), true);
        timers.process_timers(&mut wakers);
        assert_eq!(wakers.len(), 1);
        assert!(!timers.contains(first));
        assert_eq!(timers.next_latency_expiry(), None);

        std::thread::sleep(Duration::from_millis(300));
        let due = Instant::now() + Duration::from_millis(20);
        timers.insert(due, dummy_waker(), true);
        assert_eq!(timers.next_latency_expiry(), Some(due));
        wakers.clear();
        timers.process_timers(&mut wakers);
        assert!(wakers.is_empty(), "woken before it was due");
        std::thread::sleep(Duration::from_millis(25));
        timers.process_timers(&mut wakers);
        assert_eq!(wakers.len(), 1, "not woken when due");
        assert_eq!(timers.next_latency_expiry(), None);
    }

    /// Of timers that come due together, the latency ones wake first, so a
    /// latency queue enters the run queue ahead of the others woken with it.
    #[test]
    fn latency_timers_wake_first() {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut timers = ReactorTimers::new();
        let due = Instant::now();
        timers.insert(due, logging_waker(&log, 0), false);
        timers.insert(due, logging_waker(&log, 1), true);
        timers.insert(due, logging_waker(&log, 2), false);

        std::thread::sleep(Duration::from_millis(2));
        let mut wakers = Vec::new();
        timers.process_timers(&mut wakers);
        wakers.into_iter().for_each(Waker::wake);
        assert_eq!(*log.lock().unwrap(), [1, 0, 2]);
    }
}
