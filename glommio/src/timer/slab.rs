// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience
//
//! Stable storage for timer entries.
//!
//! A cascading wheel moves a timer between slots as time advances, so a handle
//! that names a wheel position stops being valid the moment the timer
//! cascades. This slab gives every timer a position that does not move for as
//! long as it lives; the wheel stores slab indices, and cascading moves an
//! index rather than an entry.
//!
//! That makes cancellation an array index instead of a hash lookup, and it
//! removes the class of bug where a handle silently addresses a different
//! timer: an index is bounded by *live* timers rather than by total
//! insertions, and a generation counter distinguishes a reused slot from the
//! one a stale handle remembers.

use std::num::NonZeroU64;

/// Position in the slab. Bounded by concurrently live timers.
///
/// Deliberately not an alias for `u32`: the defect this replaces existed
/// because a storage position and a monotonic identity were both spelled as
/// integers, so one could be assigned to the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct SlotIndex(u32);

impl SlotIndex {
    /// The slab is addressed by `u32`, so it cannot grow past `u32::MAX`
    /// entries. At the entry sizes involved that bound is hundreds of
    /// gigabytes, which is to say memory runs out first — but the conversion
    /// is checked rather than assumed, and this is the only place a `usize`
    /// becomes a `SlotIndex`.
    fn new(index: usize) -> Option<Self> {
        u32::try_from(index).ok().map(SlotIndex)
    }

    fn as_usize(self) -> usize {
        self.0 as usize
    }
}

/// Distinguishes a reused slot from the one a stale handle remembers.
///
/// `u64` rather than `u32` on purpose. A slot is reused every time a timer is
/// cancelled and another inserted, and with a LIFO free list a hot slot is
/// reused immediately — at a million operations a second a 32-bit counter
/// returns to a value a stale handle still matches in about 72 minutes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Generation(NonZeroU64);

impl Generation {
    fn first() -> Self {
        Generation(NonZeroU64::new(1).expect("1 is not zero"))
    }

    /// Exhaustion is unreachable: at a billion reuses a second this takes 584
    /// years. It is checked rather than wrapped because wrapping reissues
    /// generation 1, the value a slot's very first handle carried, which is
    /// precisely the collision generations exist to prevent.
    fn next(self) -> Self {
        let raw = self
            .0
            .get()
            .checked_add(1)
            .expect("timer slot generations exhausted");
        Generation(NonZeroU64::new(raw).expect("one more than a non-zero is non-zero"))
    }
}

/// A handle to a timer, valid until that timer is removed.
///
/// Names a slab position, not a wheel position, so cascading does not
/// invalidate it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TimerId {
    slot: SlotIndex,
    generation: Generation,
}

impl TimerId {
    /// The slab position this handle names.
    ///
    /// The wheel stores bare positions rather than whole handles: a slot holds
    /// four bytes per timer instead of twelve, so cascading a slot moves four
    /// times as many timers per cache line.
    pub(crate) fn slot(self) -> SlotIndex {
        self.slot
    }
}

#[derive(Debug)]
enum Slot<T> {
    Occupied {
        generation: Generation,
        value: T,
    },
    Vacant {
        /// Kept across the vacancy: a handle to the timer that used to live
        /// here must not match the next one that does.
        generation: Generation,
        next_free: Option<SlotIndex>,
    },
}

/// Slab of timer entries addressed by stable handles.
#[derive(Debug)]
pub(crate) struct TimerSlab<T> {
    slots: Vec<Slot<T>>,
    free_head: Option<SlotIndex>,
    live: usize,
}

impl<T> TimerSlab<T> {
    pub(crate) fn new() -> Self {
        Self {
            slots: Vec::new(),
            free_head: None,
            live: 0,
        }
    }

    /// Number of occupied slots.
    pub(crate) fn len(&self) -> usize {
        self.live
    }

    /// Store a value and return a handle to it.
    ///
    /// # Panics
    ///
    /// Panics if the slab would exceed `u32::MAX` entries, which needs that
    /// many timers alive at once.
    pub(crate) fn insert(&mut self, value: T) -> TimerId {
        self.live += 1;

        if let Some(slot_index) = self.free_head {
            let slot = &mut self.slots[slot_index.as_usize()];
            let (generation, next_free) = match slot {
                Slot::Vacant {
                    generation,
                    next_free,
                } => (generation.next(), *next_free),
                Slot::Occupied { .. } => {
                    unreachable!("free list only threads through vacant slots")
                }
            };
            *slot = Slot::Occupied { generation, value };
            self.free_head = next_free;
            return TimerId {
                slot: slot_index,
                generation,
            };
        }

        let slot_index = SlotIndex::new(self.slots.len())
            .expect("more than u32::MAX timers alive in one executor");
        let generation = Generation::first();
        self.slots.push(Slot::Occupied { generation, value });
        TimerId {
            slot: slot_index,
            generation,
        }
    }

    /// Take the value a handle names, if it is still the one that was stored.
    ///
    /// Returns `None` for a handle whose timer has already been removed, which
    /// is the ordinary case for a timer that fired before it was cancelled.
    pub(crate) fn remove(&mut self, id: TimerId) -> Option<T> {
        let slot = self.slots.get_mut(id.slot.as_usize())?;
        match slot {
            Slot::Occupied { generation, .. } if *generation == id.generation => {
                let previous = std::mem::replace(
                    slot,
                    Slot::Vacant {
                        generation: id.generation,
                        next_free: self.free_head,
                    },
                );
                self.free_head = Some(id.slot);
                self.live -= 1;
                match previous {
                    Slot::Occupied { value, .. } => Some(value),
                    Slot::Vacant { .. } => unreachable!("matched Occupied above"),
                }
            }
            _ => None,
        }
    }

    /// Borrow the value a handle names, if it is still the one that was stored.
    pub(crate) fn get(&self, id: TimerId) -> Option<&T> {
        match self.slots.get(id.slot.as_usize())? {
            Slot::Occupied { generation, value } if *generation == id.generation => Some(value),
            _ => None,
        }
    }

    /// Borrow mutably the value a handle names.
    pub(crate) fn get_mut(&mut self, id: TimerId) -> Option<&mut T> {
        match self.slots.get_mut(id.slot.as_usize())? {
            Slot::Occupied { generation, value } if *generation == id.generation => Some(value),
            _ => None,
        }
    }

    /// Look up the handle for a slot whose generation is current.
    ///
    /// The wheel stores bare `SlotIndex` values, so this is how it recovers a
    /// full handle when it needs one — on expiry, where the entry it is
    /// looking at is live by construction.
    pub(crate) fn id_at(&self, slot: SlotIndex) -> Option<TimerId> {
        match self.slots.get(slot.as_usize())? {
            Slot::Occupied { generation, .. } => Some(TimerId {
                slot,
                generation: *generation,
            }),
            Slot::Vacant { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handle_reads_back_what_was_stored() {
        let mut slab = TimerSlab::new();
        let a = slab.insert("a");
        let b = slab.insert("b");

        assert_eq!(slab.get(a), Some(&"a"));
        assert_eq!(slab.get(b), Some(&"b"));
        assert_eq!(slab.len(), 2);
    }

    #[test]
    fn removing_returns_the_value_and_retires_the_handle() {
        let mut slab = TimerSlab::new();
        let a = slab.insert("a");

        assert_eq!(slab.remove(a), Some("a"));
        assert_eq!(slab.len(), 0);
        assert_eq!(slab.get(a), None, "handle is spent");
        assert_eq!(
            slab.remove(a),
            None,
            "and removing twice is not a double free"
        );
    }

    #[test]
    fn a_reused_slot_rejects_the_previous_handle() {
        let mut slab = TimerSlab::new();
        let first = slab.insert("first");
        assert_eq!(slab.remove(first), Some("first"));

        // Reuses the slot `first` occupied.
        let second = slab.insert("second");
        assert_eq!(second.slot, first.slot, "the slot is reused");
        assert_ne!(second.generation, first.generation, "but not the identity");

        assert_eq!(slab.get(first), None, "stale handle finds nothing");
        assert_eq!(slab.remove(first), None, "and cannot cancel the new timer");
        assert_eq!(slab.get(second), Some(&"second"));
    }

    #[test]
    fn slots_are_bounded_by_live_entries_not_by_insertions() {
        let mut slab = TimerSlab::new();

        // The defect this replaces used a monotonic counter, so this loop
        // would have advanced an index a thousand times.
        for _ in 0..1_000 {
            let id = slab.insert(());
            slab.remove(id).expect("just inserted");
        }

        assert_eq!(slab.len(), 0);
        assert_eq!(slab.slots.len(), 1, "one slot, reused a thousand times");
    }

    #[test]
    fn a_handle_survives_the_value_being_mutated_in_place() {
        let mut slab = TimerSlab::new();
        let id = slab.insert(1u32);

        *slab.get_mut(id).expect("live") = 2;

        assert_eq!(slab.get(id), Some(&2));
    }

    #[test]
    fn generations_never_take_the_zero_niche() {
        // Option<Generation> is eight bytes only while zero stays unused.
        assert_eq!(
            std::mem::size_of::<Option<Generation>>(),
            std::mem::size_of::<u64>()
        );

        let mut generation = Generation::first();
        for _ in 0..1_000 {
            generation = generation.next();
        }
        assert_eq!(generation, Generation(NonZeroU64::new(1_001).unwrap()));
    }
}
