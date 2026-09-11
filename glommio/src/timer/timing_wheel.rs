// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience
//
//! Hierarchical timing wheel over stable slab handles.
//!
//! Timers live in a [`TimerSlab`]; the wheel's slots hold [`SlotIndex`] values
//! pointing into it. Cascading therefore moves four-byte indices between slots
//! rather than whole entries, and a handle stays valid for a timer's entire
//! life however many times it cascades — which is what makes cancellation a
//! bare array index rather than a hash lookup.
//!
//! Each entry records where in the wheel it currently sits, so removal goes
//! straight there. Removing from a slot is a `swap_remove`, and the entry that
//! moves into the hole has its recorded position corrected; nothing is left
//! behind to be skipped later.

use super::slab::{SlotIndex, TimerId, TimerSlab};
use std::{
    cell::Cell,
    collections::BTreeMap,
    task::Waker,
    time::{Duration, Instant},
};

/// Level 0: 256 slots x 1ms
const LEVEL_0_SLOTS: usize = 256;
/// Level 1: 64 slots x 256ms
const LEVEL_1_SLOTS: usize = 64;
const LEVEL_1_RESOLUTION_MS: u64 = 256;
/// Level 2: 64 slots x 16.384s
const LEVEL_2_SLOTS: usize = 64;
const LEVEL_2_RESOLUTION_MS: u64 = 16_384;
/// Level 3: 64 slots x 17.48min
const LEVEL_3_SLOTS: usize = 64;
const LEVEL_3_RESOLUTION_MS: u64 = 1_048_576;

/// Beyond 18 hours a timer waits in a `BTreeMap` instead of a slot.
const OVERFLOW_THRESHOLD_MS: u64 = 67_108_864;

/// Which slots of one level hold anything.
///
/// Sized for the widest level; the narrow ones simply never set the upper
/// bits. Uniform code costs 24 unused bytes per narrow level and saves a
/// second implementation of the same three operations.
#[derive(Debug, Default, Clone, Copy)]
struct SlotMask([u64; 4]);

impl SlotMask {
    fn set(&mut self, slot: usize) {
        self.0[slot / 64] |= 1 << (slot % 64);
    }

    fn clear(&mut self, slot: usize) {
        self.0[slot / 64] &= !(1 << (slot % 64));
    }

    fn is_empty(&self) -> bool {
        self.0 == [0; 4]
    }

    /// Slots occupied at or after `from`, wrapping once, searched in order.
    ///
    /// Returns how many slots ahead of `from` the first occupied one lies, so
    /// the caller can turn that into a tick without knowing the layout.
    ///
    /// A word at a time rather than a bit at a time. `next_expiry` asks level 0
    /// this on every park without first checking that anything is there, so the
    /// answer for an empty level was 256 iterations; it is now four.
    fn distance_to_next(&self, from: usize, slots: usize) -> Option<usize> {
        debug_assert!(from < slots, "slot {from} is outside a level of {slots}");
        let words = slots.div_ceil(64);
        let first = from / 64;
        let bit = from % 64;

        // One pass over the words from `from`, then the starting word once more,
        // which is where a search that wrapped ends up. That last visit needs no
        // mask of its own: reaching it means the first visit already found
        // nothing at or above `from`, so only the bits below it can be set.
        for step in 0..=words {
            let mut word = self.0[(first + step) % words];
            if step == 0 {
                word &= u64::MAX << bit;
            }
            if word != 0 {
                let slot = ((first + step) % words) * 64 + word.trailing_zeros() as usize;
                return Some((slot + slots - from) % slots);
            }
        }
        None
    }
}

/// Where an entry currently sits, so that cancelling it goes straight there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WheelPos {
    /// In a wheel slot, waiting for its level to come round.
    Slot {
        level: u8,
        slot: usize,
        index: usize,
    },
    /// Due, and waiting to be drained.
    Expired { index: usize },
    /// Too far out for the wheel; parked in the overflow map under `key`.
    Overflow { key: Instant, index: usize },
    /// Constructed but not yet given a place. Exists so "nowhere yet" is a
    /// value rather than an index that happens to be out of bounds.
    Unplaced,
}

#[derive(Debug)]
struct TimerEntry {
    expires_at: Instant,
    waker: Waker,
    at: WheelPos,
}

/// Hierarchical timing wheel with stable handles.
#[derive(Debug)]
pub(crate) struct TimingWheel {
    current_tick: u64,
    start_time: Instant,

    /// Owns every entry. Slots below hold indices into this.
    slab: TimerSlab<TimerEntry>,

    /// Due, awaiting drain.
    expired: Vec<SlotIndex>,

    slots_1ms: Box<[Vec<SlotIndex>; LEVEL_0_SLOTS]>,

    /// Cached earliest deadline per slot, one array per level, so asking what
    /// a slot holds does not walk it. `None` means unknown: set on insert,
    /// invalidated when the entry holding it leaves or the slot empties, and
    /// recomputed on the next read. Never later than the true minimum, so a
    /// timer can never be missed on the strength of it.
    earliest_1ms: Box<[Cell<Option<Instant>>; LEVEL_0_SLOTS]>,
    earliest_256ms: Box<[Cell<Option<Instant>>; LEVEL_1_SLOTS]>,
    earliest_16s: Box<[Cell<Option<Instant>>; LEVEL_2_SLOTS]>,
    earliest_17min: Box<[Cell<Option<Instant>>; LEVEL_3_SLOTS]>,
    slots_256ms: Box<[Vec<SlotIndex>; LEVEL_1_SLOTS]>,
    slots_16s: Box<[Vec<SlotIndex>; LEVEL_2_SLOTS]>,
    slots_17min: Box<[Vec<SlotIndex>; LEVEL_3_SLOTS]>,

    /// Which slots of each level hold anything, so that finding the next
    /// deadline and skipping empty time are both bounded by the number of
    /// levels rather than by the number of timers or by elapsed milliseconds.
    masks: [SlotMask; 4],

    /// Past the wheel's reach. Cold.
    overflow: BTreeMap<Instant, Vec<SlotIndex>>,

    /// Ticks actually processed. The point of the masks is that this stays
    /// unrelated to elapsed time, which is only assertable by counting.
    #[cfg(test)]
    ticks_processed: u64,
}

impl TimingWheel {
    pub(crate) fn new() -> Self {
        Self::new_at(Instant::now())
    }

    pub(crate) fn new_at(start_time: Instant) -> Self {
        Self {
            current_tick: 0,
            start_time,
            slab: TimerSlab::new(),
            expired: Vec::new(),
            slots_1ms: Box::new(std::array::from_fn(|_| Vec::new())),
            earliest_1ms: Box::new(std::array::from_fn(|_| Cell::new(None))),
            earliest_256ms: Box::new(std::array::from_fn(|_| Cell::new(None))),
            earliest_16s: Box::new(std::array::from_fn(|_| Cell::new(None))),
            earliest_17min: Box::new(std::array::from_fn(|_| Cell::new(None))),
            slots_256ms: Box::new(std::array::from_fn(|_| Vec::new())),
            slots_16s: Box::new(std::array::from_fn(|_| Vec::new())),
            slots_17min: Box::new(std::array::from_fn(|_| Vec::new())),
            masks: [SlotMask::default(); 4],
            overflow: BTreeMap::new(),
            #[cfg(test)]
            ticks_processed: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.slab.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn current_time(&self) -> Instant {
        self.start_time + Duration::from_millis(self.current_tick)
    }

    /// Earliest deadline held, if any.
    ///
    /// Never later than the truth. Sleeping past a deadline is a bug; waking
    /// early and finding nothing due costs one poll.
    pub(crate) fn next_expiry(&self) -> Option<Instant> {
        if !self.expired.is_empty() {
            return Some(self.current_time());
        }

        // Level 0 holds everything due within the next 256 ticks, and a
        // level-0 slot names exactly one tick, so its entries carry the exact
        // deadline the caller asked for.
        let from = ((self.current_tick + 1) % LEVEL_0_SLOTS as u64) as usize;
        let level_0 = self.masks[0]
            .distance_to_next(from, LEVEL_0_SLOTS)
            .and_then(|offset| {
                let slot = (from + offset) % LEVEL_0_SLOTS;
                let earliest = self.earliest_in_slot(slot);
                // The mask and the slots are two records of one fact. Were they
                // to disagree, this is where it would cost a deadline rather
                // than a wasted poll: the whole level is abandoned on the
                // nearest slot alone, never reaching the ones behind it.
                debug_assert!(
                    earliest.is_some(),
                    "level 0 slot {slot} is marked occupied but holds no deadline"
                );
                earliest
            });

        // A coarser level cannot be read without cascading it, so the bound is
        // the boundary at which that happens. One tick is subtracted because a
        // deadline is rounded up to the tick that stores it: an entry at tick
        // `t` is due anywhere in `(t - 1, t]`, so `t` itself can be later than
        // the deadline. Waking a tick early costs one poll; waking late is a
        // missed timer.
        // A coarse level is bounded by arithmetic rather than by anything
        // stored. Every entry in the slot at offset `k` from the cursor has
        // `deadline_ms / resolution == cursor_q + k`, so none can be due before
        // that product; one tick is subtracted because placement rounds up, so
        // an entry recorded at tick `t` is really due in `(t - 1, t]`.
        //
        // This is the kernel's trade at the coarse end: a bucket answers with
        // its own granularity, and precision returns when the bucket cascades
        // into level 0. Keeping a per-slot minimum here instead bought exact
        // answers at the cost of rescanning a slot whenever the entry holding
        // its minimum was withdrawn, which is what a server does to every read
        // timeout it arms.
        let coarse = [1u8, 2, 3]
            .into_iter()
            .filter(|level| !self.masks[*level as usize].is_empty())
            .filter_map(|level| {
                let (slots, resolution) = match level {
                    1 => (LEVEL_1_SLOTS, LEVEL_1_RESOLUTION_MS),
                    2 => (LEVEL_2_SLOTS, LEVEL_2_RESOLUTION_MS),
                    _ => (LEVEL_3_SLOTS, LEVEL_3_RESOLUTION_MS),
                };
                // One past the cursor, as level 0 does: the cursor slot was
                // emptied at its own boundary, so anything left in it wrapped a
                // whole revolution and holds the level's latest deadlines. The
                // scan still reaches it, last, which is its order.
                let cursor_q = self.current_tick / resolution;
                let from = ((cursor_q + 1) % slots as u64) as usize;
                let offset = self.masks[level as usize].distance_to_next(from, slots)?;
                let quotient = cursor_q + 1 + offset as u64;
                let earliest_tick = quotient * resolution;
                let bound =
                    self.start_time + Duration::from_millis(earliest_tick.saturating_sub(1));
                if bound > self.current_time() {
                    return Some(bound);
                }

                // In the last tick before this slot cascades the bound lands on
                // the cursor itself, so it says only "at or before now" and the
                // reactor would sleep for zero and wake straight back into this
                // same answer. Read the slot instead: it is one slot, and the
                // coarse sweep on this very poll has already computed and
                // cached its minimum.
                let s = (quotient % slots as u64) as usize;
                let cached = self.earliest_cell(level, s);
                if let Some(known) = cached.get() {
                    return Some(known);
                }
                let computed = self.earliest_in(self.bucket(level, s));
                cached.set(computed);
                computed
            })
            .min();

        let overflow = self.overflow.keys().next().copied();

        // The earliest of the three. Taking level 0 alone would hide a coarser
        // deadline that falls before it and is still waiting to cascade.
        [level_0, coarse, overflow].into_iter().flatten().min()
    }

    /// Expire coarse-level entries whose deadline has already passed.
    ///
    /// A deadline is rounded up to the tick that stores it, so an entry whose
    /// tick is the one a cascade lands on is only moved into level 0 at the
    /// moment it is already due, and would be found a tick late. The cached
    /// minimum makes the usual answer a single comparison per level; the slot
    /// is only walked when it really holds something due.
    fn expire_due_in_coarse_levels(&mut self, now: Instant) {
        // The tick `now` rounds up to, matching how a deadline is placed: an
        // entry due at this moment is the one stored at that tick.
        let now_tick = match now.checked_duration_since(self.start_time) {
            Some(elapsed) => elapsed.as_nanos().div_ceil(1_000_000).min(u64::MAX as u128) as u64,
            None => return,
        };

        for (level, resolution, slots) in [
            (1u8, LEVEL_1_RESOLUTION_MS, LEVEL_1_SLOTS),
            (2, LEVEL_2_RESOLUTION_MS, LEVEL_2_SLOTS),
            (3, LEVEL_3_RESOLUTION_MS, LEVEL_3_SLOTS),
        ] {
            let slot = ((now_tick / resolution) % slots as u64) as usize;
            // The cached minimum is never later than the truth, so one later
            // than `now` proves nothing in the slot is due and the rebuild is
            // skipped. Without this every poll in the millisecond before a
            // cascade rebuilt the whole slot.
            if let Some(earliest) = self.earliest_cell(level, slot).get() {
                if earliest > now {
                    continue;
                }
            }

            let held = std::mem::take(self.bucket_mut(level, slot));
            let mut survivor_min: Option<Instant> = None;
            let mut retained = Vec::with_capacity(held.len());
            for index in held {
                let Some(id) = self.slab.id_at(index) else {
                    continue;
                };
                let due = self.slab.get(id).is_some_and(|e| e.expires_at <= now);
                if due {
                    self.expired.push(index);
                    let at = WheelPos::Expired {
                        index: self.expired.len() - 1,
                    };
                    self.record(id, at);
                } else {
                    if let Some(entry) = self.slab.get(id) {
                        survivor_min = Some(match survivor_min {
                            Some(m) => m.min(entry.expires_at),
                            None => entry.expires_at,
                        });
                    }
                    retained.push(index);
                    let at = WheelPos::Slot {
                        level,
                        slot,
                        index: retained.len() - 1,
                    };
                    self.record(id, at);
                }
            }
            if retained.is_empty() {
                self.masks[level as usize].clear(slot);
            }
            *self.bucket_mut(level, slot) = retained;
            // The walk just computed the exact minimum of what is left, so
            // store it rather than discarding it and rescanning next poll.
            self.earliest_cell(level, slot).set(survivor_min);
        }
    }

    /// The earliest deadline in a level-0 slot, from the cache where possible.
    ///
    /// Filling a stale entry costs one scan of that slot; every read until the
    /// entry holding the minimum leaves is then free.
    fn earliest_in_slot(&self, slot: usize) -> Option<Instant> {
        if let Some(cached) = self.earliest_1ms[slot].get() {
            return Some(cached);
        }
        let computed = self.earliest_in(&self.slots_1ms[slot]);
        self.earliest_1ms[slot].set(computed);
        computed
    }

    /// The earliest deadline among the entries a slot holds.
    fn earliest_in(&self, slot: &[SlotIndex]) -> Option<Instant> {
        slot.iter()
            .filter_map(|index| self.slab.id_at(*index))
            .filter_map(|id| self.slab.get(id))
            .map(|entry| entry.expires_at)
            .min()
    }

    /// The next tick at which a coarser level must be broken down, if any
    /// coarser level holds anything.
    fn next_cascade_tick(&self) -> Option<u64> {
        [
            (1usize, LEVEL_1_RESOLUTION_MS),
            (2, LEVEL_2_RESOLUTION_MS),
            (3, LEVEL_3_RESOLUTION_MS),
        ]
        .into_iter()
        .filter(|(level, _)| !self.masks[*level].is_empty())
        .map(|(_, resolution)| (self.current_tick / resolution + 1) * resolution)
        .min()
    }

    /// The next tick at or before `limit` at which the wheel has work.
    fn next_work_tick(&self, limit: u64) -> Option<u64> {
        let from = ((self.current_tick + 1) % LEVEL_0_SLOTS as u64) as usize;
        let level_0 = self.masks[0]
            .distance_to_next(from, LEVEL_0_SLOTS)
            .map(|offset| self.current_tick + 1 + offset as u64);

        [level_0, self.next_cascade_tick()]
            .into_iter()
            .flatten()
            .min()
            .filter(|tick| *tick <= limit)
    }

    /// Whether a handle still names a live timer.
    pub(crate) fn contains(&self, id: TimerId) -> bool {
        self.slab.get(id).is_some()
    }

    /// Register a timer, returning a handle valid until it fires or is removed.
    pub(crate) fn insert(&mut self, expires_at: Instant, waker: Waker) -> TimerId {
        let id = self.slab.insert(TimerEntry {
            expires_at,
            waker,
            // `place` below gives it a position; until then it has none.
            at: WheelPos::Unplaced,
        });
        self.place(id);
        id
    }

    /// Withdraw a timer, handing back its waker. `None` if it has already
    /// fired or was already withdrawn.
    pub(crate) fn remove(&mut self, id: TimerId) -> Option<Waker> {
        let entry = self.slab.get(id)?;
        let at = entry.at;
        // Only the entry that supplied a slot's cached minimum can invalidate
        // it. Cancelling any of the others leaves the cache correct, which
        // matters because withdrawing a timer is the common case.
        if let WheelPos::Slot { level, slot, .. } = at {
            let cached = self.earliest_cell(level, slot);
            if cached.get() == Some(entry.expires_at) {
                cached.set(None);
            }
        }
        self.unlink(at, id.slot());
        self.slab.remove(id).map(|entry| entry.waker)
    }

    /// Move time forward, expiring whatever has come due.
    ///
    /// Cost is proportional to the ticks actually crossed. Skipping empty
    /// spans is a separate change; see [`Self::next_expiry`].
    pub(crate) fn advance_to(&mut self, now: Instant) {
        if now <= self.start_time {
            return;
        }

        let target_tick = now
            .duration_since(self.start_time)
            .as_millis()
            .min(u64::MAX as u128) as u64;

        // Step to the next tick that has work rather than through every
        // millisecond between. An executor that idles for ten minutes with a
        // populated wheel would otherwise pay six hundred thousand iterations
        // on its next poll, for time in which nothing was due.
        while self.current_tick < target_tick {
            match self.next_work_tick(target_tick) {
                Some(tick) => {
                    self.current_tick = tick - 1;
                    self.tick();
                }
                None => {
                    self.current_tick = target_tick;
                    break;
                }
            }
        }

        self.expire_due_before(now);
        self.expire_due_in_coarse_levels(now);
        self.check_overflow();
    }

    /// Expire entries in the next slot whose real deadline has already passed.
    ///
    /// Deadlines round up to a whole tick so nothing fires early, which puts a
    /// timer due at 1.7ms in tick 2 -- and the tick sweep alone would not
    /// reach it until 2.0ms. `next_expiry` reports 1.7ms, so the reactor wakes
    /// then, and this is what finds it. Without it the wheel's resolution
    /// becomes a floor under every sleep.
    fn expire_due_before(&mut self, now: Instant) {
        let slot = ((self.current_tick + 1) % LEVEL_0_SLOTS as u64) as usize;

        // Cheap check first, and `advance_to` runs on every poll rather than
        // only before a park, so this is the hottest read in the wheel. The
        // cached minimum is never later than the truth, so one later than
        // `now` proves nothing in the slot is due and the walk is skipped.
        if self
            .earliest_in_slot(slot)
            .is_none_or(|earliest| earliest > now)
        {
            return;
        }

        let held = std::mem::take(&mut self.slots_1ms[slot]);
        let mut retained = Vec::with_capacity(held.len());
        for index in held {
            let Some(id) = self.slab.id_at(index) else {
                continue;
            };
            let due = self
                .slab
                .get(id)
                .is_some_and(|entry| entry.expires_at <= now);

            if due {
                self.expired.push(index);
                let at = WheelPos::Expired {
                    index: self.expired.len() - 1,
                };
                self.record(id, at);
            } else {
                retained.push(index);
                let at = WheelPos::Slot {
                    level: 0,
                    slot,
                    index: retained.len() - 1,
                };
                self.record(id, at);
            }
        }

        if retained.is_empty() {
            self.masks[0].clear(slot);
        }
        self.slots_1ms[slot] = retained;
        // Entries left, so whatever was cached may name one of them.
        self.earliest_1ms[slot].set(None);
    }

    /// Take everything that has come due, appending it to `out`.
    ///
    /// Both buffers keep their allocation: `out` belongs to the caller and is
    /// reused across polls, and the pending list is cleared rather than taken,
    /// so a batch of expiries costs no allocation once the wheel is warm.
    /// Indexing rather than iterating is what allows the slab to be mutated in
    /// the same loop; the entries are four bytes and `Copy`.
    pub(crate) fn drain_expired_into(&mut self, out: &mut Vec<(TimerId, Waker)>) {
        let due = self.expired.len();
        for i in 0..due {
            let slot = self.expired[i];
            if let Some(id) = self.slab.id_at(slot) {
                if let Some(entry) = self.slab.remove(id) {
                    out.push((id, entry.waker));
                }
            }
        }
        // The bound above was taken once. Nothing on this path re-places a
        // timer, but were that to change, the entries appended behind us would
        // be cleared without ever being woken.
        debug_assert_eq!(
            self.expired.len(),
            due,
            "the pending list grew while draining"
        );
        self.expired.clear();
    }

    /// Everything that has come due, as an iterator.
    ///
    /// The allocation this makes is why the reactor uses
    /// [`Self::drain_expired_into`] instead.
    #[cfg(test)]
    pub(crate) fn drain_expired(&mut self) -> std::vec::IntoIter<(TimerId, Waker)> {
        let mut out = Vec::new();
        self.drain_expired_into(&mut out);
        out.into_iter()
    }

    // ---- placement -------------------------------------------------------

    /// Put an already-stored entry wherever its deadline says it belongs, and
    /// record where that was.
    fn place(&mut self, id: TimerId) {
        let expires_at = self
            .slab
            .get(id)
            .expect("caller just stored this entry")
            .expires_at;

        let deadline_ms = match expires_at.checked_duration_since(self.start_time) {
            // Round up. `as_millis` truncates, which places a timer due at
            // 1.7ms in tick 1 and fires it 0.7ms early -- before its own
            // deadline, so the future polls, finds itself not ready, and has to
            // arm again. A timer may fire late; it may never fire early.
            Some(duration) => duration
                .as_nanos()
                .div_ceil(1_000_000)
                .min(u64::MAX as u128) as u64,
            // Already in the past.
            None => return self.mark_expired(id),
        };

        if deadline_ms <= self.current_tick {
            return self.mark_expired(id);
        }

        let ticks_until_expiry = deadline_ms - self.current_tick;
        let slot = id.slot();

        let at = if ticks_until_expiry >= OVERFLOW_THRESHOLD_MS {
            let bucket = self.overflow.entry(expires_at).or_default();
            bucket.push(slot);
            WheelPos::Overflow {
                key: expires_at,
                index: bucket.len() - 1,
            }
        } else {
            let (level, s) = if ticks_until_expiry < LEVEL_1_RESOLUTION_MS {
                (0u8, (deadline_ms % LEVEL_0_SLOTS as u64) as usize)
            } else if ticks_until_expiry < LEVEL_2_RESOLUTION_MS {
                (
                    1,
                    ((deadline_ms / LEVEL_1_RESOLUTION_MS) % LEVEL_1_SLOTS as u64) as usize,
                )
            } else if ticks_until_expiry < LEVEL_3_RESOLUTION_MS {
                (
                    2,
                    ((deadline_ms / LEVEL_2_RESOLUTION_MS) % LEVEL_2_SLOTS as u64) as usize,
                )
            } else {
                (
                    3,
                    ((deadline_ms / LEVEL_3_RESOLUTION_MS) % LEVEL_3_SLOTS as u64) as usize,
                )
            };

            // Only ever tighten a known minimum. `None` means unknown, not
            // empty: an entry earlier than this one may already be in the slot,
            // and claiming this deadline as the minimum would hide it and let
            // the reactor sleep past it. Leaving it unknown costs one recompute
            // on the next read.
            let cached = self.earliest_cell(level, s);
            if let Some(current) = cached.get() {
                cached.set(Some(current.min(expires_at)));
            }

            let bucket = self.bucket_mut(level, s);
            bucket.push(slot);
            let index = bucket.len() - 1;
            self.masks[level as usize].set(s);
            WheelPos::Slot {
                level,
                slot: s,
                index,
            }
        };

        self.record(id, at);
    }

    fn mark_expired(&mut self, id: TimerId) {
        self.expired.push(id.slot());
        let at = WheelPos::Expired {
            index: self.expired.len() - 1,
        };
        self.record(id, at);
    }

    fn record(&mut self, id: TimerId, at: WheelPos) {
        if let Some(entry) = self.slab.get_mut(id) {
            entry.at = at;
        }
    }

    /// Detach an entry from wherever it sits, correcting whatever moves into
    /// the hole it leaves.
    /// `expect` is the entry being detached. Nothing verifies a recorded
    /// position as it is written, and a wrong one does not simply miss: the
    /// slab mints a removal handle from whatever index it finds, so a stale
    /// position detaches whichever timer occupies that place now. This is the
    /// one point where the entry and its claimed position are both in hand.
    fn unlink(&mut self, at: WheelPos, expect: SlotIndex) {
        let (moved_slot, corrected) = match at {
            // Nothing to detach: it was never given a place.
            WheelPos::Unplaced => return,
            WheelPos::Slot { level, slot, index } => {
                let bucket = self.bucket_mut(level, slot);
                if index >= bucket.len() {
                    return;
                }
                debug_assert_eq!(
                    bucket.get(index).copied(),
                    Some(expect),
                    "level {level} slot {slot} index {index} holds another entry"
                );
                bucket.swap_remove(index);
                let moved = bucket.get(index).copied();
                let emptied = bucket.is_empty();
                if emptied {
                    self.masks[level as usize].clear(slot);
                    // An empty slot has no minimum. Leaving the old value would
                    // have the next insert take a minimum against a deadline
                    // that no longer exists.
                    self.earliest_cell(level, slot).set(None);
                }
                (moved, WheelPos::Slot { level, slot, index })
            }
            WheelPos::Expired { index } => {
                if index >= self.expired.len() {
                    return;
                }
                self.expired.swap_remove(index);
                let moved = self.expired.get(index).copied();
                (moved, WheelPos::Expired { index })
            }
            WheelPos::Overflow { key, index } => {
                let Some(bucket) = self.overflow.get_mut(&key) else {
                    return;
                };
                if index >= bucket.len() {
                    return;
                }
                bucket.swap_remove(index);
                let moved = bucket.get(index).copied();
                if bucket.is_empty() {
                    self.overflow.remove(&key);
                }
                (moved, WheelPos::Overflow { key, index })
            }
        };

        // `swap_remove` moved the last element into the hole. Its recorded
        // position now lies, so correct it before anything reads it.
        if let Some(moved) = moved_slot {
            if let Some(id) = self.slab.id_at(moved) {
                self.record(id, corrected);
            }
        }
    }

    fn earliest_cell(&self, level: u8, slot: usize) -> &Cell<Option<Instant>> {
        match level {
            0 => &self.earliest_1ms[slot],
            1 => &self.earliest_256ms[slot],
            2 => &self.earliest_16s[slot],
            3 => &self.earliest_17min[slot],
            _ => unreachable!("wheel has four levels"),
        }
    }

    fn bucket(&self, level: u8, slot: usize) -> &[SlotIndex] {
        match level {
            0 => &self.slots_1ms[slot],
            1 => &self.slots_256ms[slot],
            2 => &self.slots_16s[slot],
            3 => &self.slots_17min[slot],
            _ => unreachable!("wheel has four levels"),
        }
    }

    fn bucket_mut(&mut self, level: u8, slot: usize) -> &mut Vec<SlotIndex> {
        match level {
            0 => &mut self.slots_1ms[slot],
            1 => &mut self.slots_256ms[slot],
            2 => &mut self.slots_16s[slot],
            3 => &mut self.slots_17min[slot],
            _ => unreachable!("wheel has four levels"),
        }
    }

    // ---- time ------------------------------------------------------------

    fn tick(&mut self) {
        self.current_tick += 1;
        #[cfg(test)]
        {
            self.ticks_processed += 1;
        }

        let slot_0 = (self.current_tick % LEVEL_0_SLOTS as u64) as usize;
        self.expire_slot(slot_0);

        if self.current_tick.is_multiple_of(LEVEL_1_RESOLUTION_MS) {
            let s = ((self.current_tick / LEVEL_1_RESOLUTION_MS) % LEVEL_1_SLOTS as u64) as usize;
            self.cascade_slot(1, s);
        }
        if self.current_tick.is_multiple_of(LEVEL_2_RESOLUTION_MS) {
            let s = ((self.current_tick / LEVEL_2_RESOLUTION_MS) % LEVEL_2_SLOTS as u64) as usize;
            self.cascade_slot(2, s);
        }
        if self.current_tick.is_multiple_of(LEVEL_3_RESOLUTION_MS) {
            let s = ((self.current_tick / LEVEL_3_RESOLUTION_MS) % LEVEL_3_SLOTS as u64) as usize;
            self.cascade_slot(3, s);
        }

        self.check_overflow();
    }

    /// Everything in a level-0 slot is due.
    fn expire_slot(&mut self, slot: usize) {
        let due = std::mem::take(&mut self.slots_1ms[slot]);
        self.masks[0].clear(slot);
        self.earliest_1ms[slot].set(None);
        for entry_slot in due {
            self.expired.push(entry_slot);
            let at = WheelPos::Expired {
                index: self.expired.len() - 1,
            };
            if let Some(id) = self.slab.id_at(entry_slot) {
                self.record(id, at);
            }
        }
    }

    /// Re-place a higher level's slot into finer levels.
    fn cascade_slot(&mut self, level: u8, slot: usize) {
        let moving = std::mem::take(self.bucket_mut(level, slot));
        self.masks[level as usize].clear(slot);
        self.earliest_cell(level, slot).set(None);
        for entry_slot in moving {
            if let Some(id) = self.slab.id_at(entry_slot) {
                self.place(id);
            }
        }
    }

    /// Pull overflow entries that have come within the wheel's reach.
    fn check_overflow(&mut self) {
        let threshold = self.current_time() + Duration::from_millis(OVERFLOW_THRESHOLD_MS);

        // Strictly earlier than the threshold `place` evicts at. Reclaiming the
        // boundary itself would hand back an entry that `place` immediately
        // returns to the map, once per poll for as long as the cursor sits in
        // that tick.
        let due: Vec<Instant> = self
            .overflow
            .range(..threshold)
            .map(|(key, _)| *key)
            .collect();
        if due.is_empty() {
            return;
        }

        for key in due {
            if let Some(bucket) = self.overflow.remove(&key) {
                for entry_slot in bucket {
                    if let Some(id) = self.slab.id_at(entry_slot) {
                        self.place(id);
                    }
                }
            }
        }
    }
}

impl Default for TimingWheel {
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
    fn a_sub_tick_deadline_is_reported_and_expired_at_its_real_time() {
        // The wheel's resolution must not become a floor under every sleep.
        // A caller asking for 300us gets 300us, not the millisecond the tick
        // it landed in ends at.
        let start = Instant::now();
        let mut wheel = TimingWheel::new_at(start);
        wheel.insert(start + Duration::from_micros(300), dummy_waker());

        assert_eq!(
            wheel.next_expiry(),
            Some(start + Duration::from_micros(300)),
            "reported the tick boundary instead of the deadline asked for"
        );

        wheel.advance_to(start + Duration::from_micros(300));
        assert_eq!(wheel.drain_expired().count(), 1, "due at 300us");
    }

    #[test]
    fn a_sub_tick_deadline_still_does_not_expire_early() {
        let start = Instant::now();
        let mut wheel = TimingWheel::new_at(start);
        wheel.insert(start + Duration::from_micros(300), dummy_waker());

        wheel.advance_to(start + Duration::from_micros(299));
        assert_eq!(wheel.drain_expired().count(), 0, "not due at 299us");
    }

    #[test]
    fn crossing_idle_time_costs_nothing_when_there_is_no_work() {
        let start = Instant::now();
        let mut wheel = TimingWheel::new_at(start);

        // An hour of nothing. Stepping through it a millisecond at a time
        // would be 3.6 million iterations.
        wheel.advance_to(start + Duration::from_secs(3600));

        assert_eq!(wheel.ticks_processed, 0, "no work, so no ticks");
        assert_eq!(wheel.current_tick, 3_600_000, "but time still moved");
    }

    #[test]
    fn crossing_idle_time_costs_the_work_not_the_interval() {
        let start = Instant::now();
        let mut wheel = TimingWheel::new_at(start);
        wheel.insert(start + Duration::from_secs(3600), dummy_waker());

        wheel.advance_to(start + Duration::from_secs(3540));

        assert!(
            wheel.ticks_processed < 1_000,
            "processed {} ticks to cross 59 minutes holding one timer",
            wheel.ticks_processed
        );
        assert_eq!(wheel.drain_expired().count(), 0, "not due yet");
    }

    #[test]
    fn cancelling_the_last_timer_in_a_slot_empties_the_level() {
        let start = Instant::now();
        let mut wheel = TimingWheel::new_at(start);
        let id = wheel.insert(start + Duration::from_millis(50), dummy_waker());

        assert!(wheel.next_expiry().is_some(), "a deadline is pending");
        assert!(wheel.remove(id).is_some());
        assert_eq!(
            wheel.next_expiry(),
            None,
            "a slot whose last timer was cancelled must stop being reported \
             as occupied, or the reactor wakes for a timer that is not there"
        );

        wheel.advance_to(start + Duration::from_millis(50));
        assert_eq!(wheel.ticks_processed, 0, "and there was nothing to do");
    }

    #[test]
    fn a_deadline_between_ticks_does_not_expire_at_the_earlier_one() {
        // A deadline is rounded up to a whole tick, never down. Truncating
        // places a timer due at 1.7ms in tick 1 and expires it at 1.0ms --
        // before it is due. The future then finds itself not ready and arms
        // again, so one sleep costs several registrations.
        //
        // Checked here rather than through the executor because the reactor
        // sleeps until the true deadline when nothing else is running, which
        // hides it; and because inline storage compares instants exactly, so
        // it only appears once the staged wheel has promoted.
        let start = Instant::now();
        let mut wheel = TimingWheel::new_at(start);
        wheel.insert(start + Duration::from_micros(1_700), dummy_waker());

        wheel.advance_to(start + Duration::from_millis(1));
        assert_eq!(
            wheel.drain_expired().count(),
            0,
            "expired at 1ms a timer that is due at 1.7ms"
        );

        wheel.advance_to(start + Duration::from_millis(2));
        assert_eq!(wheel.drain_expired().count(), 1, "due by 2ms");
    }

    #[test]
    fn test_basic_insert_and_expire() {
        let start = Instant::now();
        let mut wheel = TimingWheel::new_at(start);

        // Insert timer expiring in 100ms
        let id = wheel.insert(start + Duration::from_millis(100), dummy_waker());

        assert_eq!(wheel.len(), 1);

        // Advance past expiry
        wheel.advance_to(start + Duration::from_millis(100));

        // Should have one expired timer
        let expired: Vec<_> = wheel.drain_expired().collect();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].0, id);

        // Wheel should be empty now
        assert_eq!(wheel.len(), 0);
    }

    #[test]
    fn test_remove_timer() {
        let start = Instant::now();
        let mut wheel = TimingWheel::new_at(start);

        let id = wheel.insert(start + Duration::from_millis(100), dummy_waker());
        assert_eq!(wheel.len(), 1);

        // Remove the timer
        assert!(wheel.remove(id).is_some());
        assert_eq!(wheel.len(), 0);

        // Advance time - should not expire
        wheel.advance_to(start + Duration::from_millis(100));
        assert_eq!(wheel.drain_expired().count(), 0);
    }

    #[test]
    fn test_multiple_timers_same_slot() {
        let start = Instant::now();
        let mut wheel = TimingWheel::new_at(start);

        // Insert 3 timers at same expiry
        let id1 = wheel.insert(start + Duration::from_millis(50), dummy_waker());
        let id2 = wheel.insert(start + Duration::from_millis(50), dummy_waker());
        let id3 = wheel.insert(start + Duration::from_millis(50), dummy_waker());

        assert_eq!(wheel.len(), 3);

        wheel.advance_to(start + Duration::from_millis(50));

        let expired: Vec<_> = wheel.drain_expired().map(|(id, _)| id).collect();
        assert_eq!(expired.len(), 3);
        assert!(expired.contains(&id1));
        assert!(expired.contains(&id2));
        assert!(expired.contains(&id3));
    }

    #[test]
    fn test_timer_ordering() {
        let start = Instant::now();
        let mut wheel = TimingWheel::new_at(start);

        // Insert timers in reverse order
        wheel.insert(start + Duration::from_millis(30), dummy_waker());
        wheel.insert(start + Duration::from_millis(10), dummy_waker());
        wheel.insert(start + Duration::from_millis(20), dummy_waker());

        // Advance to 10ms
        wheel.advance_to(start + Duration::from_millis(10));
        assert_eq!(wheel.drain_expired().count(), 1);

        // Advance to 20ms
        wheel.advance_to(start + Duration::from_millis(20));
        assert_eq!(wheel.drain_expired().count(), 1);

        // Advance to 30ms
        wheel.advance_to(start + Duration::from_millis(30));
        assert_eq!(wheel.drain_expired().count(), 1);
    }

    #[test]
    fn test_cascading_level_1_to_0() {
        let start = Instant::now();
        let mut wheel = TimingWheel::new_at(start);

        // Insert timer at 500ms (will be in Level 1)
        let id = wheel.insert(start + Duration::from_millis(500), dummy_waker());

        // Advance to just before Level 1 cascade (255ms)
        wheel.advance_to(start + Duration::from_millis(255));
        assert_eq!(wheel.drain_expired().count(), 0);

        // Advance to 256ms - should cascade from Level 1 to Level 0
        wheel.advance_to(start + Duration::from_millis(256));
        assert_eq!(wheel.drain_expired().count(), 0); // Not expired yet

        // Advance to 500ms - should expire
        wheel.advance_to(start + Duration::from_millis(500));
        let expired: Vec<_> = wheel.drain_expired().collect();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].0, id);
    }

    #[test]
    fn test_long_duration_timer() {
        let start = Instant::now();
        let mut wheel = TimingWheel::new_at(start);

        // Insert timer at 1 hour (will be in Level 3)
        let id = wheel.insert(start + Duration::from_secs(3600), dummy_waker());

        assert_eq!(wheel.len(), 1);

        // Advance to expiry
        wheel.advance_to(start + Duration::from_secs(3600));

        let expired: Vec<_> = wheel.drain_expired().collect();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].0, id);
    }

    #[test]
    fn test_overflow_to_btreemap() {
        let start = Instant::now();
        let mut wheel = TimingWheel::new_at(start);

        // Insert timer at 24 hours (should overflow to BTreeMap)
        let id = wheel.insert(start + Duration::from_secs(86400), dummy_waker());

        assert_eq!(wheel.len(), 1);

        // Advance to expiry
        wheel.advance_to(start + Duration::from_secs(86400));

        let expired: Vec<_> = wheel.drain_expired().collect();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].0, id);
    }

    #[test]
    fn test_past_timer_expires_immediately() {
        let start = Instant::now();
        let mut wheel = TimingWheel::new_at(start);

        // Insert timer in the past
        wheel.insert(start - Duration::from_millis(100), dummy_waker());

        // Should expire immediately
        assert_eq!(wheel.drain_expired().count(), 1);
    }

    #[test]
    fn test_wrap_around() {
        let start = Instant::now();
        let mut wheel = TimingWheel::new_at(start);

        // Insert timer at slot 10
        let id1 = wheel.insert(start + Duration::from_millis(10), dummy_waker());

        // Advance to slot 260 (wraps around Level 0)
        wheel.advance_to(start + Duration::from_millis(260));

        // Insert another timer at slot 10 (should be different from id1)
        let id2 = wheel.insert(start + Duration::from_millis(260 + 10), dummy_waker());

        assert_ne!(id1, id2);

        // First timer should have expired
        assert!(wheel.drain_expired().any(|(id, _)| id == id1));

        // Advance to second timer
        wheel.advance_to(start + Duration::from_millis(270));
        assert!(wheel.drain_expired().any(|(id, _)| id == id2));
    }
    #[test]
    fn earlier_coarse_timer_must_not_be_hidden_by_fine_timer() {
        let start = Instant::now();
        let mut wheel = TimingWheel::new_at(start);
        wheel.insert(start + Duration::from_millis(300), Waker::noop().clone());
        wheel.advance_to(start + Duration::from_millis(250));
        wheel.insert(start + Duration::from_millis(400), Waker::noop().clone());
        let next = wheel.next_expiry().unwrap().duration_since(start);
        assert!(
            next <= Duration::from_millis(300),
            "next wake is {next:?}, missing deadline at 300ms"
        );
    }

    #[test]
    fn a_fractional_deadline_in_a_coarse_level_fires_on_time() {
        // 255.5ms rounds up to tick 256, one past the level-0 window, so it
        // waits in a coarse level for a cascade that lands on the very tick it
        // is due. Finding it only then would fire it a fraction of a tick late.
        let start = Instant::now();
        let mut wheel = TimingWheel::new_at(start);
        let deadline = start + Duration::from_micros(255_500);
        wheel.insert(deadline, Waker::noop().clone());
        wheel.advance_to(deadline);
        assert_eq!(
            wheel.drain_expired().count(),
            1,
            "a deadline that has arrived was left waiting for its cascade"
        );
    }

    #[test]
    fn a_fractional_deadline_in_a_coarse_level_is_not_slept_past() {
        // 255.5ms rounds to tick 256, which is one past the level-0 window, so
        // the deadline is only reachable through a cascade. Reporting the
        // cascade boundary itself would name a time later than the deadline.
        let start = Instant::now();
        let mut wheel = TimingWheel::new_at(start);
        let deadline = start + Duration::from_micros(255_500);
        wheel.insert(deadline, Waker::noop().clone());
        let next = wheel.next_expiry().unwrap();
        assert!(
            next <= deadline,
            "next wake is {:?} past the deadline",
            next.duration_since(deadline)
        );
    }
    #[test]
    fn cache_must_not_hide_an_earlier_deadline_after_invalidation() {
        let start = Instant::now();
        let mut wheel = TimingWheel::new_at(start);
        // A and B share a level-0 slot; B is the minimum.
        let _a = wheel.insert(start + Duration::from_micros(1_100), Waker::noop().clone());
        let b = wheel.insert(start + Duration::from_micros(1_050), Waker::noop().clone());
        // Withdrawing B invalidates the cached minimum; A is still there.
        wheel.remove(b);
        // C is later than A, same slot.
        let _c = wheel.insert(start + Duration::from_micros(1_900), Waker::noop().clone());

        let next = wheel.next_expiry().unwrap();
        let a_due = start + Duration::from_micros(1_100);
        assert!(
            next <= a_due,
            "next_expiry is {:?} past a deadline the wheel still holds",
            next.duration_since(a_due)
        );
    }
    #[test]
    fn coarse_timer_must_not_make_next_expiry_land_in_the_past() {
        let start = Instant::now();
        let mut wheel = TimingWheel::new_at(start);
        // A single 10s timer: lives in a coarse level for a long time.
        wheel.insert(start + Duration::from_secs(10), Waker::noop().clone());
        // Step to the tick just before a level-1 cascade boundary (256ms).
        wheel.advance_to(start + Duration::from_millis(255));
        let now = wheel.current_time();
        let next = wheel.next_expiry().unwrap();
        assert!(
            next > now,
            "next_expiry is {:?} in the PAST at tick 255, so the reactor sleeps zero and spins",
            now.duration_since(next)
        );
    }
    #[test]
    fn coarse_search_must_not_start_on_the_emptied_cursor_slot() {
        let start = Instant::now();
        let mut wheel = TimingWheel::new_at(start);
        wheel.advance_to(start + Duration::from_millis(255));
        wheel.insert(
            start + Duration::from_millis(255 + 16_383),
            Waker::noop().clone(),
        );
        wheel.insert(
            start + Duration::from_millis(255 + 300),
            Waker::noop().clone(),
        );
        let next = wheel.next_expiry().unwrap();
        let truth = start + Duration::from_millis(555);
        assert!(
            next <= truth,
            "next_expiry is {:?} past the earliest deadline",
            next.duration_since(truth)
        );
    }

    #[test]
    fn a_coarse_timer_alone_in_the_cursor_slot_is_still_found() {
        let start = Instant::now();
        let mut wheel = TimingWheel::new_at(start);
        wheel.advance_to(start + Duration::from_millis(255));
        wheel.insert(
            start + Duration::from_millis(255 + 16_383),
            Waker::noop().clone(),
        );
        assert!(
            wheel.next_expiry().is_some(),
            "skipping the cursor slot lost the only timer in the wheel"
        );
    }
    #[test]
    fn a_coarse_level_never_reports_a_deadline_already_past() {
        // In the tick before a coarse slot cascades, the bucket bound lands on
        // the cursor. Answering with it means naming a time at or before now,
        // which the reactor turns into a zero-length sleep that wakes straight
        // back into the same answer, for the rest of the millisecond.
        let start = Instant::now();
        let mut wheel = TimingWheel::new_at(start);
        wheel.insert(start + Duration::from_millis(500), Waker::noop().clone());

        let now = start + Duration::from_micros(255_001);
        wheel.advance_to(now);
        assert_eq!(wheel.drain_expired().count(), 0, "nothing is due at 255ms");

        let next = wheel.next_expiry().expect("the timer is still held");
        assert!(
            next > now,
            "reported {:?} before now, so the reactor sleeps zero and spins",
            now.duration_since(next)
        );
    }

    #[test]
    fn distance_to_next_agrees_with_the_scan_it_replaced() {
        // The word-at-a-time search is only worth having if it answers exactly
        // what the bit-at-a-time one did, so the old body is the oracle.
        fn scan(mask: &SlotMask, from: usize, slots: usize) -> Option<usize> {
            (0..slots).find(|offset| {
                let slot = (from + offset) % slots;
                mask.0[slot / 64] & (1 << (slot % 64)) != 0
            })
        }

        fn xorshift(state: &mut u64) -> u64 {
            let mut x = *state;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *state = x;
            x
        }

        for &slots in &[LEVEL_1_SLOTS, LEVEL_0_SLOTS] {
            // Empty, and every single occupied slot on its own.
            let empty = SlotMask::default();
            for from in 0..slots {
                assert_eq!(empty.distance_to_next(from, slots), None);
            }
            for occupied in 0..slots {
                let mut mask = SlotMask::default();
                mask.set(occupied);
                for from in 0..slots {
                    assert_eq!(
                        mask.distance_to_next(from, slots),
                        scan(&mask, from, slots),
                        "slots {slots}, only {occupied} occupied, searching from {from}"
                    );
                }
            }

            // Then arbitrary populations, including the full one.
            let mut rng = 0x9E37_79B9_7F4A_7C15u64;
            for case in 0..200 {
                let mut mask = SlotMask::default();
                for slot in 0..slots {
                    if case == 0 || xorshift(&mut rng) % 4 == 0 {
                        mask.set(slot);
                    }
                }
                for from in 0..slots {
                    assert_eq!(
                        mask.distance_to_next(from, slots),
                        scan(&mask, from, slots),
                        "slots {slots}, case {case}, searching from {from}"
                    );
                }
            }
        }
    }

    /// Both halves of the wheel's contract, over randomized traffic.
    ///
    /// A timer may fire late; it may never fire early. And `next_expiry` may
    /// name a time earlier than the truth, costing a wasted poll; it may never
    /// name one later, which would sleep past a deadline.
    #[test]
    fn contract_holds_under_randomized_traffic() {
        fn xorshift(state: &mut u64) -> u64 {
            let mut x = *state;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *state = x;
            x
        }

        for seed in 1..=200u64 {
            let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            let start = Instant::now();
            let mut wheel = TimingWheel::new_at(start);
            let mut live: Vec<(TimerId, Instant)> = Vec::new();
            let mut now = start;

            for step in 0..400 {
                match xorshift(&mut rng) % 10 {
                    0..=4 => {
                        // Three bands, because one modulus cannot reach both
                        // ends: under 90s never places anything in level 3 or
                        // the overflow map, and drawing only from days means
                        // nothing ever fires inside a run. Sub-millisecond
                        // fractions throughout, so rounding is exercised.
                        let micros = match xorshift(&mut rng) % 3 {
                            0 => xorshift(&mut rng) % 4_000,
                            1 => xorshift(&mut rng) % 90_000_000,
                            _ => xorshift(&mut rng) % 200_000_000_000,
                        };
                        let deadline = now + Duration::from_micros(micros);
                        let id = wheel.insert(deadline, Waker::noop().clone());
                        live.push((id, deadline));
                    }
                    5..=6 if !live.is_empty() => {
                        let i = (xorshift(&mut rng) % live.len() as u64) as usize;
                        let (id, _) = live.swap_remove(i);
                        wheel.remove(id);
                    }
                    _ => {
                        // Deliberately fine as well as coarse: a step that
                        // lands inside the millisecond a deadline sits in is
                        // the only thing that catches a timer firing early.
                        let step_us = match xorshift(&mut rng) % 3 {
                            0 => xorshift(&mut rng) % 900,
                            1 => xorshift(&mut rng) % 50_000,
                            _ => xorshift(&mut rng) % 3_000_000,
                        };
                        now += Duration::from_micros(step_us);
                        wheel.advance_to(now);
                        let drained: Vec<_> = wheel.drain_expired().collect();
                        // Nothing is due and nothing is waiting to be drained,
                        // so any deadline the wheel names has to be in the
                        // future. Naming the present or the past makes the
                        // reactor sleep for zero and wake straight back into
                        // the same answer.
                        if let Some(next) = wheel.next_expiry() {
                            assert!(
                                next > now,
                                "seed {seed} step {step}: next_expiry is {:?} in the PAST",
                                now.duration_since(next)
                            );
                        }
                        for (id, _) in drained {
                            let pos = live.iter().position(|(l, _)| *l == id);
                            if let Some(pos) = pos {
                                let (_, deadline) = live.swap_remove(pos);
                                assert!(
                                    deadline <= now,
                                    "seed {seed} step {step}: fired {:?} EARLY",
                                    deadline.duration_since(now)
                                );
                            }
                        }
                    }
                }

                if let (Some(next), Some(truth)) =
                    (wheel.next_expiry(), live.iter().map(|(_, d)| *d).min())
                {
                    assert!(
                        next <= truth,
                        "seed {seed} step {step}: next_expiry is {:?} LATER than a live deadline",
                        next.duration_since(truth)
                    );
                }
            }
        }
    }
}
