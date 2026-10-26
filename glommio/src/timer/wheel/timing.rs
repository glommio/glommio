//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! The wheel itself.

use super::{
    mask::SlotMask,
    policy::{validate, CoarseQuery, Overflow, Policy, Rounding},
    slab::{Key, Slab, SlotIndex},
};
use std::{
    cell::Cell,
    collections::BTreeMap,
    marker::PhantomData,
    time::{Duration, Instant},
};

/// Where an entry currently sits, so that cancelling it goes straight there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Position {
    Slot {
        level: usize,
        slot: usize,
        index: usize,
    },
    Expired {
        index: usize,
    },
    Overflow {
        key: Instant,
        index: usize,
    },
    /// Constructed but not yet given a place. Exists so "nowhere yet" is a
    /// value rather than an index that happens to be out of bounds.
    Unplaced,
}

#[derive(Debug)]
struct Entry<T> {
    expires_at: Instant,
    payload: T,
    at: Position,
}

#[derive(Debug)]
struct Level {
    slots: Box<[Vec<SlotIndex>]>,
    mask: SlotMask,
    /// `log2` of how many ticks one slot spans, so dividing by it is a shift.
    shift: u32,
    /// `slots - 1`, so taking a slot number is a mask.
    wrap: usize,
    /// Earliest deadline per slot, under `CoarseQuery::ExactMinimum` or for
    /// the finest level, which is read before every sleep. `None` means
    /// unknown rather than empty: an entry earlier than the one being recorded
    /// may already be present, and claiming otherwise hides it.
    earliest: Option<Box<[Cell<Option<Instant>>]>>,
}

/// A hierarchical timing wheel with stable handles.
///
/// Entries live in a slab and the slots hold indices into it, so cascading
/// moves four-byte indices rather than whole entries, and a handle stays valid
/// for an entry's entire life however many times it moves between slots.
#[derive(Debug)]
pub struct TimingWheel<T, P: Policy> {
    current_tick: u64,
    start: Instant,
    /// The instant last handed to [`TimingWheel::advance_to`].
    ///
    /// `start + current_tick` is only accurate to a whole tick, and a bound
    /// can legitimately fall between that and the caller's real time. Sleeping
    /// on such an answer wakes immediately and asks the same question again, so
    /// the wheel keeps what it was actually told.
    advanced_to: Instant,
    slab: Slab<Entry<T>>,
    expired: Vec<SlotIndex>,
    levels: Vec<Level>,
    overflow: BTreeMap<Instant, Vec<SlotIndex>>,
    /// Ticks in one full revolution of the coarsest level, which is everything
    /// the wheel can address.
    reach: u64,
    policy: PhantomData<P>,
}

impl<T, P: Policy> TimingWheel<T, P> {
    /// Builds a wheel whose time starts now.
    pub fn new() -> Self {
        Self::starting_at(Instant::now())
    }

    /// Builds a wheel whose time starts at `start`.
    ///
    /// The wheel never reads the clock: `advance_to` is told what time it is.
    /// That is what makes it testable without waiting, and it means a caller
    /// with its own clock can drive it.
    pub fn starting_at(start: Instant) -> Self {
        validate::<P>();
        let levels: Vec<Level> = P::SLOTS
            .iter()
            .zip(P::RESOLUTION)
            .enumerate()
            .map(|(level, (&slots, &resolution))| Level {
                slots: vec![Vec::new(); slots].into_boxed_slice(),
                mask: SlotMask::new(slots),
                shift: resolution.trailing_zeros(),
                wrap: slots - 1,
                earliest: (level == 0 || P::COARSE_QUERY == CoarseQuery::ExactMinimum)
                    .then(|| vec![Cell::new(None); slots].into_boxed_slice()),
            })
            .collect();

        let reach = P::RESOLUTION[P::RESOLUTION.len() - 1] * P::SLOTS[P::SLOTS.len() - 1] as u64;

        Self {
            current_tick: 0,
            start,
            advanced_to: start,
            slab: Slab::new(),
            expired: Vec::new(),
            levels,
            overflow: BTreeMap::new(),
            reach,
            policy: PhantomData,
        }
    }

    /// How many entries the wheel is holding, expired or not.
    pub fn len(&self) -> usize {
        self.slab.len()
    }

    /// Whether the wheel holds nothing at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The wheel's own idea of the current time, to the tick.
    ///
    /// Quantised, so it is at or before [`TimingWheel::last_advanced`]. The
    /// wheel never reads a clock; both of these come from what
    /// [`TimingWheel::advance_to`] was told.
    pub fn current_time(&self) -> Instant {
        self.start + Duration::from_millis(self.current_tick)
    }

    /// The instant last handed to [`TimingWheel::advance_to`].
    pub fn last_advanced(&self) -> Instant {
        self.advanced_to
    }

    /// Whether a handle still names a live entry.
    pub fn contains(&self, key: Key) -> bool {
        self.slab.get(key).is_some()
    }

    /// The payload behind a handle, without withdrawing it.
    pub fn get(&self, key: Key) -> Option<&T> {
        self.slab.get(key).map(|entry| &entry.payload)
    }

    /// The payload behind a handle, mutably.
    ///
    /// The deadline is deliberately not reachable this way: the wheel filed
    /// the entry by it, so changing it would leave the entry in the wrong
    /// slot. Withdraw and re-register to move a deadline.
    pub fn get_mut(&mut self, key: Key) -> Option<&mut T> {
        self.slab.get_mut(key).map(|entry| &mut entry.payload)
    }

    /// When an entry is due, if it is still held.
    pub fn deadline(&self, key: Key) -> Option<Instant> {
        self.slab.get(key).map(|entry| entry.expires_at)
    }

    /// Registers an entry, returning a handle valid until it expires or is
    /// withdrawn.
    ///
    /// # Panics
    ///
    /// Only under [`Overflow::Reject`], and only for a deadline past the
    /// wheel's reach. The default policy parks such a deadline instead and
    /// this cannot fail, which is why it does not return an `Option`: a caller
    /// who has not opted into refusal should not have to unwrap one forever.
    /// Use [`TimingWheel::try_insert`] where refusal is possible.
    pub fn insert(&mut self, expires_at: Instant, payload: T) -> Key {
        self.try_insert(expires_at, payload).expect(
            "a deadline beyond the wheel's reach was refused; \
             Overflow::Reject is in force, so use try_insert",
        )
    }

    /// Registers an entry, or refuses it.
    ///
    /// `None` only when the deadline is beyond the wheel's reach and the
    /// policy is [`Overflow::Reject`].
    pub fn try_insert(&mut self, expires_at: Instant, payload: T) -> Option<Key> {
        let deadline = self.tick_for(expires_at);
        if P::OVERFLOW == Overflow::Reject
            && deadline.is_some_and(|tick| tick.saturating_sub(self.current_tick) >= self.reach)
        {
            return None;
        }
        let key = self.slab.insert(Entry {
            expires_at,
            payload,
            at: Position::Unplaced,
        });
        self.place_at(key, deadline);
        Some(key)
    }

    /// Withdraws an entry, handing back its payload.
    ///
    /// Forgets its slot's cached minimum only when this entry supplied it.
    pub fn remove(&mut self, key: Key) -> Option<T> {
        let entry = self.slab.get(key)?;
        let at = entry.at;
        let expires_at = entry.expires_at;
        if let Position::Slot { level, slot, .. } = at {
            if let Some(cell) = self.earliest_cell(level, slot) {
                if cell.get() == Some(expires_at) {
                    cell.set(None);
                }
            }
        }
        self.unlink(at, key.slot());
        self.slab.remove(key).map(|entry| entry.payload)
    }

    fn earliest_cell(&self, level: usize, slot: usize) -> Option<&Cell<Option<Instant>>> {
        self.levels[level].earliest.as_ref().map(|e| &e[slot])
    }
}

impl<T, P: Policy> Default for TimingWheel<T, P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, P: Policy> TimingWheel<T, P> {
    /// The tick a deadline is stored at, under the configured rounding.
    ///
    /// A wheel's resolution is one tick, so a deadline of 1.7 ticks has to be
    /// kept at a whole one. Which way it goes decides whether an entry can
    /// expire before the caller asked for it, which is why it is a policy and
    /// not a detail.
    fn tick_for(&self, expires_at: Instant) -> Option<u64> {
        let elapsed = expires_at.checked_duration_since(self.start)?;
        let nanos = elapsed.as_nanos();
        const PER_TICK: u128 = 1_000_000;
        let ticks = match P::ROUNDING {
            Rounding::Up => nanos.div_ceil(PER_TICK),
            Rounding::Down => nanos / PER_TICK,
            Rounding::Nearest => (nanos + PER_TICK / 2) / PER_TICK,
        };
        Some(ticks.min(u64::MAX as u128) as u64)
    }

    /// Puts an already-stored entry wherever its deadline says it belongs, and
    /// records where that was.
    fn place(&mut self, key: Key) {
        let expires_at = self
            .slab
            .get(key)
            .expect("caller just stored this entry")
            .expires_at;
        let deadline = self.tick_for(expires_at);
        self.place_at(key, deadline);
    }

    /// Places an entry whose tick the caller has already worked out, in the
    /// finest level whose reach still covers it.
    ///
    /// A slot's cached minimum is only ever tightened here. An unknown one
    /// stays unknown, since an earlier entry may already be in the slot.
    fn place_at(&mut self, key: Key, deadline: Option<u64>) {
        let expires_at = self
            .slab
            .get(key)
            .expect("caller just stored this entry")
            .expires_at;

        let Some(deadline) = deadline else {
            return self.mark_expired(key);
        };
        if deadline <= self.current_tick {
            return self.mark_expired(key);
        }

        let remaining = deadline - self.current_tick;
        let slot_index = key.slot();

        if remaining >= self.reach {
            let bucket = self.overflow.entry(expires_at).or_default();
            bucket.push(slot_index);
            let at = Position::Overflow {
                key: expires_at,
                index: bucket.len() - 1,
            };
            return self.record(key, at);
        }

        let level = self
            .levels
            .iter()
            .position(|l| remaining < (1u64 << l.shift) * (l.wrap as u64 + 1))
            .unwrap_or(self.levels.len() - 1);
        let slot = ((deadline >> self.levels[level].shift) as usize) & self.levels[level].wrap;

        if let Some(cell) = self.earliest_cell(level, slot) {
            if let Some(current) = cell.get() {
                cell.set(Some(current.min(expires_at)));
            }
        }

        let bucket = &mut self.levels[level].slots[slot];
        bucket.push(slot_index);
        let index = bucket.len() - 1;
        self.levels[level].mask.set(slot);
        self.record(key, Position::Slot { level, slot, index });
    }

    fn mark_expired(&mut self, key: Key) {
        self.expired.push(key.slot());
        let at = Position::Expired {
            index: self.expired.len() - 1,
        };
        self.record(key, at);
    }

    fn record(&mut self, key: Key, at: Position) {
        if let Some(entry) = self.slab.get_mut(key) {
            entry.at = at;
        }
    }

    /// Detaches an entry from wherever it sits, correcting the recorded
    /// position of whatever `swap_remove` moves into the hole it leaves.
    ///
    /// A slot left empty also forgets its cached minimum, so the next
    /// insertion does not tighten against a deadline that is no longer there.
    ///
    /// `expect` is the entry being detached. Nothing verifies a recorded
    /// position as it is written, and a wrong one does not merely miss: the
    /// slab mints a removal handle from whatever index it finds, so a stale
    /// position detaches whichever entry occupies that place now. So every
    /// branch checks it, in release builds as well: a cancel pays one
    /// comparison, and the alternative is cancelling another caller's timer.
    fn unlink(&mut self, at: Position, expect: SlotIndex) {
        let (moved, corrected) = match at {
            Position::Unplaced => return,
            Position::Slot { level, slot, index } => {
                let bucket = &mut self.levels[level].slots[slot];
                if index >= bucket.len() {
                    return;
                }
                assert_eq!(bucket.get(index).copied(), Some(expect));
                bucket.swap_remove(index);
                let moved = bucket.get(index).copied();
                if bucket.is_empty() {
                    self.levels[level].mask.clear(slot);
                    if let Some(cell) = self.earliest_cell(level, slot) {
                        cell.set(None);
                    }
                }
                (moved, Position::Slot { level, slot, index })
            }
            Position::Expired { index } => {
                if index >= self.expired.len() {
                    return;
                }
                assert_eq!(self.expired.get(index).copied(), Some(expect));
                self.expired.swap_remove(index);
                (
                    self.expired.get(index).copied(),
                    Position::Expired { index },
                )
            }
            Position::Overflow { key, index } => {
                let Some(bucket) = self.overflow.get_mut(&key) else {
                    return;
                };
                if index >= bucket.len() {
                    return;
                }
                assert_eq!(bucket.get(index).copied(), Some(expect));
                bucket.swap_remove(index);
                let moved = bucket.get(index).copied();
                if bucket.is_empty() {
                    self.overflow.remove(&key);
                }
                (moved, Position::Overflow { key, index })
            }
        };

        if let Some(moved) = moved {
            if let Some(key) = self.slab.id_at(moved) {
                self.record(key, corrected);
            }
        }
    }
}

impl<T, P: Policy> TimingWheel<T, P> {
    /// The earliest deadline held, if any.
    ///
    /// Never later than the earliest deadline as it was asked for, which under
    /// [`Rounding::Down`] or [`Rounding::Nearest`] can be up to a tick after
    /// the entry is due. A caller that sleeps on this answer must be able to
    /// trust that bound in one direction only: waking early costs one wasted
    /// poll, waking late is a missed deadline. Under
    /// [`CoarseQuery::BucketBound`] the answer for a coarse level is the
    /// earliest time anything in it *could* be due, which is deliberately
    /// early rather than exact.
    pub fn next_expiry(&self) -> Option<Instant> {
        if !self.expired.is_empty() {
            return Some(self.current_time());
        }

        let from_levels = (0..self.levels.len())
            .filter_map(|level| self.next_in_level(level))
            .min();
        let from_overflow = self.overflow.keys().next().copied();

        [from_levels, from_overflow].into_iter().flatten().min()
    }

    /// The earliest time anything in a level could be due.
    ///
    /// The search starts one past the cursor: the cursor's own slot was emptied
    /// at its boundary, so anything left in it wrapped a whole revolution and
    /// holds the level's latest deadlines, not its earliest.
    ///
    /// Without a cached minimum the answer is the tick the slot's bucket
    /// begins at, less the rounding slack of [`TimingWheel::tick_instant`]. In
    /// the last tick before the slot breaks down that bound is at or before
    /// now, which would have a caller sleep for zero and wake into the same
    /// answer, so the slot is scanned instead.
    fn next_in_level(&self, level: usize) -> Option<Instant> {
        let l = &self.levels[level];
        if l.mask.is_empty() {
            return None;
        }
        let cursor = (self.current_tick >> l.shift) as usize;
        let from = (cursor + 1) & l.wrap;
        let offset = l.mask.distance_to_next(from)?;
        let slot = (from + offset) & l.wrap;

        let exact = level == 0 || P::COARSE_QUERY == CoarseQuery::ExactMinimum;
        if exact {
            return self.earliest_in_slot(level, slot);
        }

        let quotient = ((self.current_tick >> l.shift) + 1 + offset as u64) << l.shift;
        let bound = self.tick_instant(quotient);
        if bound > self.advanced_to {
            return Some(bound);
        }
        self.earliest_in_slot(level, slot)
    }

    /// The earliest real time an entry recorded at `tick` could be due.
    ///
    /// Placement moves a deadline to a whole tick, so the recorded tick is not
    /// the deadline: how far below it the truth can lie is exactly what the
    /// rounding policy decided. A bound that forgets that is a bound that can
    /// name a time later than a live deadline, which is the one direction a
    /// caller sleeping on this answer cannot survive.
    ///
    /// Rounded up, the truth lies anywhere in the tick below. Rounded to the
    /// nearer tick, it can have come up from half a tick below. Rounded down,
    /// it is at or after the recorded tick.
    fn tick_instant(&self, tick: u64) -> Instant {
        let slack_nanos = match P::ROUNDING {
            Rounding::Up => 1_000_000,
            Rounding::Nearest => 500_000,
            Rounding::Down => 0,
        };
        (self.start + Duration::from_millis(tick)) - Duration::from_nanos(slack_nanos)
    }

    /// The earliest deadline in one slot, from the cached minimum when the
    /// level keeps one.
    fn earliest_in_slot(&self, level: usize, slot: usize) -> Option<Instant> {
        if let Some(cell) = self.earliest_cell(level, slot) {
            if let Some(known) = cell.get() {
                return Some(known);
            }
            let computed = self.scan_slot(level, slot);
            cell.set(computed);
            return computed;
        }
        self.scan_slot(level, slot)
    }

    fn scan_slot(&self, level: usize, slot: usize) -> Option<Instant> {
        self.levels[level].slots[slot]
            .iter()
            .filter_map(|index| self.slab.id_at(*index))
            .filter_map(|key| self.slab.get(key))
            .map(|entry| entry.expires_at)
            .min()
    }
}

impl<T, P: Policy> TimingWheel<T, P> {
    /// Moves time forward, expiring whatever has come due.
    ///
    /// Cost is the work crossed, not the interval: an idle wheel skips to the
    /// next tick that has something to do rather than stepping through every
    /// tick between.
    ///
    /// The overflow map is reclaimed before the final sweep for entries whose
    /// tick has not arrived. An entry coming back from it can already be due,
    /// and a sweep that ran first would leave it in a level with a past
    /// deadline, which `next_expiry` would then report. Wheels with a short
    /// reach send almost everything through the overflow map, so for them this
    /// is the common path.
    pub fn advance_to(&mut self, now: Instant) {
        if now <= self.start {
            return;
        }
        self.advanced_to = now.max(self.advanced_to);
        let target = now
            .duration_since(self.start)
            .as_millis()
            .min(u64::MAX as u128) as u64;

        while self.current_tick < target {
            match self.next_work_tick(target) {
                Some(tick) => {
                    self.current_tick = tick - 1;
                    self.tick();
                }
                None => {
                    self.current_tick = target;
                    break;
                }
            }
        }

        self.check_overflow();
        self.expire_due(now);
    }

    /// The next tick at or before `limit` at which the wheel has work: the
    /// finest level's next occupied slot, or the boundary where an occupied
    /// coarser level breaks down.
    fn next_work_tick(&self, limit: u64) -> Option<u64> {
        let finest = &self.levels[0];
        let from = ((self.current_tick + 1) & finest.wrap as u64) as usize;
        let level_0 = finest
            .mask
            .distance_to_next(from)
            .map(|offset| self.current_tick + 1 + offset as u64);

        let cascade = (1..self.levels.len())
            .filter(|level| !self.levels[*level].mask.is_empty())
            .map(|level| {
                let span = 1u64 << self.levels[level].shift;
                (self.current_tick / span + 1) * span
            })
            .min();

        [level_0, cascade]
            .into_iter()
            .flatten()
            .min()
            .filter(|tick| *tick <= limit)
    }

    fn tick(&mut self) {
        self.current_tick += 1;

        let slot = (self.current_tick & self.levels[0].wrap as u64) as usize;
        self.expire_slot(slot);

        for level in 1..self.levels.len() {
            let span = 1u64 << self.levels[level].shift;
            if self.current_tick.is_multiple_of(span) {
                let slot = ((self.current_tick >> self.levels[level].shift) as usize)
                    & self.levels[level].wrap;
                self.cascade(level, slot);
            }
        }

        self.check_overflow();
    }

    /// Everything in a slot of the finest level is due.
    fn expire_slot(&mut self, slot: usize) {
        let due = std::mem::take(&mut self.levels[0].slots[slot]);
        self.levels[0].mask.clear(slot);
        if let Some(cell) = self.earliest_cell(0, slot) {
            cell.set(None);
        }
        for index in due {
            self.expired.push(index);
            let at = Position::Expired {
                index: self.expired.len() - 1,
            };
            if let Some(key) = self.slab.id_at(index) {
                self.record(key, at);
            }
        }
    }

    /// Breaks a coarser level's slot down into finer ones.
    fn cascade(&mut self, level: usize, slot: usize) {
        let moving = std::mem::take(&mut self.levels[level].slots[slot]);
        self.levels[level].mask.clear(slot);
        if let Some(cell) = self.earliest_cell(level, slot) {
            cell.set(None);
        }
        for index in moving {
            if let Some(key) = self.slab.id_at(index) {
                self.place(key);
            }
        }
    }

    /// Expires entries whose real deadline has passed but whose tick has not
    /// arrived, at every level.
    ///
    /// Deadlines are stored at whole ticks, so under [`Rounding::Up`] an entry
    /// due at 1.7 ticks sits at tick 2 and the tick sweep alone would not
    /// reach it until 2.0. Without this the wheel's resolution becomes a floor
    /// under every wait. Only the slot `now` rounds into can hold such an
    /// entry.
    fn expire_due(&mut self, now: Instant) {
        let Some(rounded) = self.tick_for(now) else {
            return;
        };
        for level in 0..self.levels.len() {
            let slot = ((rounded >> self.levels[level].shift) as usize) & self.levels[level].wrap;
            self.expire_due_in(level, slot, now);
        }
    }

    /// Expires everything in one slot whose real deadline has passed.
    ///
    /// The cached minimum is never later than the truth, so one later than
    /// `now` proves nothing here is due and the walk is skipped.
    fn expire_due_in(&mut self, level: usize, slot: usize, now: Instant) {
        if self
            .earliest_cell(level, slot)
            .map(|cell| cell.get().is_some_and(|earliest| earliest > now))
            .unwrap_or(false)
        {
            return;
        }

        let held = std::mem::take(&mut self.levels[level].slots[slot]);
        if held.is_empty() {
            return;
        }
        let mut survivor_min: Option<Instant> = None;
        let mut retained = Vec::with_capacity(held.len());
        for index in held {
            let Some(key) = self.slab.id_at(index) else {
                continue;
            };
            let due = self.slab.get(key).is_some_and(|e| e.expires_at <= now);
            if due {
                self.expired.push(index);
                let at = Position::Expired {
                    index: self.expired.len() - 1,
                };
                self.record(key, at);
            } else {
                if let Some(entry) = self.slab.get(key) {
                    survivor_min = Some(match survivor_min {
                        Some(m) => m.min(entry.expires_at),
                        None => entry.expires_at,
                    });
                }
                retained.push(index);
                let at = Position::Slot {
                    level,
                    slot,
                    index: retained.len() - 1,
                };
                self.record(key, at);
            }
        }
        if retained.is_empty() {
            self.levels[level].mask.clear(slot);
        }
        self.levels[level].slots[slot] = retained;
        if let Some(cell) = self.earliest_cell(level, slot) {
            cell.set(survivor_min);
        }
    }

    /// Pulls overflow entries that have come within the wheel's reach.
    ///
    /// Only those strictly earlier than the threshold placement evicts at.
    /// Reclaiming the boundary itself would hand back an entry that placement
    /// returns at once, on every call for as long as the cursor sits there.
    fn check_overflow(&mut self) {
        if self.overflow.is_empty() {
            return;
        }
        let threshold = self.current_time() + Duration::from_millis(self.reach);
        let due: Vec<Instant> = self.overflow.range(..threshold).map(|(k, _)| *k).collect();
        for key in due {
            if let Some(bucket) = self.overflow.remove(&key) {
                for index in bucket {
                    if let Some(k) = self.slab.id_at(index) {
                        self.place(k);
                    }
                }
            }
        }
    }

    /// Takes everything that has come due, appending it to `out`.
    ///
    /// Neither buffer gives up its allocation: `out` belongs to the caller and
    /// is reused across polls, and the pending list is cleared rather than
    /// taken. A caller draining on every poll should prefer this to
    /// [`TimingWheel::drain_expired`], which allocates a vector per call to
    /// hand back an iterator.
    ///
    /// Nothing on this path may register an entry: the pending list is cleared
    /// at the end, so anything appended while draining would be lost.
    pub fn drain_expired_into(&mut self, out: &mut Vec<(Key, T)>) {
        let due = self.expired.len();
        for i in 0..due {
            let index = self.expired[i];
            if let Some(key) = self.slab.id_at(index) {
                if let Some(entry) = self.slab.remove(key) {
                    out.push((key, entry.payload));
                }
            }
        }
        debug_assert_eq!(
            self.expired.len(),
            due,
            "the pending list grew while draining"
        );
        self.expired.clear();
    }

    /// Takes everything that has come due.
    ///
    /// Allocates a vector per call. [`TimingWheel::drain_expired_into`] does
    /// not.
    pub fn drain_expired(&mut self) -> impl Iterator<Item = (Key, T)> + '_ {
        let mut out = Vec::new();
        self.drain_expired_into(&mut out);
        out.into_iter()
    }
}
