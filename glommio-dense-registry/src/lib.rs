//! A packed registry with drop-based removal and snapshot mutation semantics.

use std::{
    cell::RefCell,
    mem,
    rc::{Rc, Weak},
};

/// Iterates directly over a dense vector while deferring mutations to its boundary.
/// Insertion and deferred mutations are amortized O(1); immediate removal and
/// `shuffle_first` are O(1), excluding value destructors. Iteration order is unspecified.
/// Separate slot and reverse-index tables keep registrations valid when values move,
/// without metadata in the iteration path. Unique registrations allow free slots to be
/// reused safely. `S` selects `u8`, `u16`, `u32`, or `usize` storage for every slot
/// ID and dense index; it defaults to `u8`. Each registry owns its random state.
pub struct DenseRegistry<T, S: SlotId = u8> {
    state: RefCell<State<T, S>>,
}

/// Integer widths supported by registry slot and dense-index tables.
pub trait SlotId: Copy + TryFrom<usize> + TryInto<usize> {
    /// Converts a valid dense index or panics if the selected width is exhausted.
    fn from_index(index: usize) -> Self {
        Self::try_from(index)
            .ok()
            .expect("dense registry slot ID capacity exhausted")
    }

    /// Converts a stored slot ID to a vector index.
    fn index(self) -> usize {
        self.try_into().ok().expect("slot ID exceeds usize")
    }
}

impl SlotId for u8 {}
impl SlotId for u16 {}
impl SlotId for u32 {}
impl SlotId for usize {}

struct State<T, S> {
    entries: Option<Entries<T, S>>,
    slots: Vec<Slot<S>>,
    free: Option<S>,
    pending: Vec<Mutation<T, S>>,
    random: fastrand::Rng,
}

struct Entries<T, S> {
    values: Vec<T>,
    slots: Vec<S>,
}

enum Slot<S> {
    Occupied(S),
    Reserved,
    Free(Option<S>),
}

enum Mutation<T, S> {
    Insert(S, T),
    Remove(S),
}

/// Removing the unique registration releases its value at the next batch boundary.
pub struct Registration<T, S: SlotId = usize> {
    registry: Weak<DenseRegistry<T, S>>,
    slot: S,
}

impl<T, S: SlotId> Default for DenseRegistry<T, S> {
    fn default() -> Self {
        Self {
            state: RefCell::new(State {
                entries: Some(Entries {
                    values: Vec::new(),
                    slots: Vec::new(),
                }),
                slots: Vec::new(),
                free: None,
                pending: Vec::new(),
                random: fastrand::Rng::new(),
            }),
        }
    }
}

impl<T, S: SlotId> DenseRegistry<T, S> {
    /// Inserts into a randomly chosen position, swapping its previous occupant to the end.
    /// Returns the value in `Err` without inserting if every slot ID is in use.
    /// The full integer range is available, so `u8` permits 256 registrations.
    /// Deferred removals release their slot IDs at the batch boundary, even if their
    /// handles have already been dropped.
    pub fn insert(self: &Rc<Self>, value: T) -> Result<Registration<T, S>, T> {
        let slot = self.state.borrow_mut().insert(value)?;
        Ok(Registration {
            registry: Rc::downgrade(self),
            slot,
        })
    }

    /// Visits the current values without registry borrows or per-entry bookkeeping.
    /// Mutations take effect after the visit, including when the visitor unwinds.
    /// Recursive iteration panics.
    pub fn for_each(&self, mut visit: impl FnMut(&mut T)) {
        let entries = self
            .state
            .borrow_mut()
            .entries
            .take()
            .expect("recursive registry iteration");
        let mut batch = Batch {
            registry: self,
            entries: Some(entries),
        };
        for value in &mut batch.entries.as_mut().unwrap().values {
            visit(value);
        }
    }

    /// Swaps the first value with a random different position, if one exists.
    /// Panics if called during iteration.
    pub fn shuffle_first(&self) {
        let mut state = self.state.borrow_mut();
        let State {
            entries,
            slots,
            random,
            ..
        } = &mut *state;
        let entries = entries.as_mut().expect("cannot shuffle during iteration");
        if entries.values.len() > 1 {
            let other = random.usize(1..entries.values.len());
            entries.values.swap(0, other);
            entries.slots.swap(0, other);
            slots[entries.slots[0].index()] = Slot::Occupied(S::from_index(0));
            slots[entries.slots[other].index()] = Slot::Occupied(S::from_index(other));
        }
    }

    fn remove(&self, slot: S) {
        let removed = {
            let mut state = self.state.borrow_mut();
            let State {
                entries,
                slots,
                free,
                ..
            } = &mut *state;
            if let Some(entries) = entries {
                Some(entries.remove(slots, free, slot))
            } else {
                state.pending.push(Mutation::Remove(slot));
                None
            }
        };
        drop(removed);
    }
}

impl<T, S: SlotId> State<T, S> {
    fn insert(&mut self, value: T) -> Result<S, T> {
        let slot = if let Some(slot) = self.free {
            let Slot::Free(next) = self.slots[slot.index()] else {
                unreachable!()
            };
            self.free = next;
            self.slots[slot.index()] = Slot::Reserved;
            slot
        } else {
            let Ok(slot) = S::try_from(self.slots.len()) else {
                return Err(value);
            };
            self.slots.push(Slot::Reserved);
            slot
        };
        if let Some(entries) = &mut self.entries {
            entries.insert(&mut self.slots, &mut self.random, slot, value);
        } else {
            self.pending.push(Mutation::Insert(slot, value));
        }
        Ok(slot)
    }
}

impl<T, S: SlotId> Entries<T, S> {
    fn insert(&mut self, slots: &mut [Slot<S>], random: &mut fastrand::Rng, slot: S, value: T) {
        let index = random.usize(..=self.values.len());
        self.values.push(value);
        self.slots.push(slot);
        let last = self.values.len() - 1;
        self.values.swap(index, last);
        self.slots.swap(index, last);
        slots[self.slots[last].index()] = Slot::Occupied(S::from_index(last));
        slots[slot.index()] = Slot::Occupied(S::from_index(index));
    }

    fn remove(&mut self, slots: &mut [Slot<S>], free: &mut Option<S>, slot: S) -> T {
        let Slot::Occupied(index) = slots[slot.index()] else {
            unreachable!()
        };
        let index = index.index();
        let value = self.values.swap_remove(index);
        self.slots.swap_remove(index);
        if index < self.values.len() {
            slots[self.slots[index].index()] = Slot::Occupied(S::from_index(index));
        }
        slots[slot.index()] = Slot::Free(free.replace(slot));
        value
    }
}

impl<T, S: SlotId> Drop for Registration<T, S> {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            registry.remove(self.slot);
        }
    }
}

struct Batch<'a, T, S: SlotId> {
    registry: &'a DenseRegistry<T, S>,
    entries: Option<Entries<T, S>>,
}

impl<T, S: SlotId> Drop for Batch<'_, T, S> {
    fn drop(&mut self) {
        let mut entries = self.entries.take().unwrap();
        let mut removed = Vec::new();
        {
            let mut state = self.registry.state.borrow_mut();
            for mutation in mem::take(&mut state.pending) {
                match mutation {
                    Mutation::Insert(slot, value) => {
                        let State { slots, random, .. } = &mut *state;
                        entries.insert(slots, random, slot, value);
                    }
                    Mutation::Remove(slot) => {
                        let State { slots, free, .. } = &mut *state;
                        removed.push(entries.remove(slots, free, slot));
                    }
                }
            }
            state.entries = Some(entries);
        }
        drop(removed);
    }
}

#[cfg(kani)]
mod proofs {
    //! Kani checks the production code with every legal random index in each harness.

    use std::{
        cell::RefCell,
        ops::{Bound, RangeBounds},
        rc::Rc,
    };

    use super::{DenseRegistry, Entries, Registration, Slot, SlotId, State};

    fn seeded_rng() -> fastrand::Rng {
        fastrand::Rng::with_seed(0)
    }

    fn arbitrary_index<R: RangeBounds<usize>>(_: &mut fastrand::Rng, range: R) -> usize {
        let first = match range.start_bound() {
            Bound::Included(&value) => value,
            Bound::Excluded(&value) => value + 1,
            Bound::Unbounded => 0,
        };
        let last = match range.end_bound() {
            Bound::Included(&value) => value,
            Bound::Excluded(&value) => value - 1,
            Bound::Unbounded => usize::MAX,
        };
        assert!(first <= last && last <= usize::from(u8::MAX));
        if first == last {
            return first;
        }
        let index = usize::from(kani::any::<u8>());
        kani::assume(range.contains(&index));
        index
    }

    /// Generates every bijection of three dense indices and three slot IDs.
    fn entries<S: SlotId>() -> (Entries<u8, S>, Vec<Slot<S>>) {
        let keys: [u8; 3] = kani::any();
        for index in 0..3 {
            kani::assume(keys[index] < 3);
            for other in index + 1..3 {
                kani::assume(keys[index] != keys[other]);
            }
        }
        let entries = Entries {
            values: vec![0, 1, 2],
            slots: keys
                .into_iter()
                .map(|key| S::from_index(usize::from(key)))
                .collect(),
        };
        let mut slots = vec![
            Slot::Reserved,
            Slot::Reserved,
            Slot::Reserved,
            Slot::Reserved,
        ];
        for (index, slot) in entries.slots.iter().enumerate() {
            slots[slot.index()] = Slot::Occupied(S::from_index(index));
        }
        (entries, slots)
    }

    fn assert_bijection<S: SlotId>(entries: &Entries<u8, S>, slots: &[Slot<S>]) {
        assert_eq!(entries.values.len(), entries.slots.len());
        for (index, slot) in entries.slots.iter().enumerate() {
            let Slot::Occupied(mapped) = slots[slot.index()] else {
                panic!("dense value lost its slot");
            };
            assert_eq!(mapped.index(), index);
        }
        for (slot, entry) in slots.iter().enumerate() {
            if let Slot::Occupied(index) = entry {
                assert_eq!(entries.slots[index.index()].index(), slot);
            }
        }
    }

    fn insert_preserves_ownership<S: SlotId>() {
        let (mut entries, mut slots) = entries::<S>();
        let expected = [
            entries.slots[0].index(),
            entries.slots[1].index(),
            entries.slots[2].index(),
            3,
        ];
        entries.insert(&mut slots, &mut seeded_rng(), S::from_index(3), 3);
        assert_eq!(entries.values.len(), 4);
        assert_bijection(&entries, &slots);
        let mut seen = [false; 4];
        for (index, value) in entries.values.iter().enumerate() {
            let value = usize::from(*value);
            assert!(!seen[value]);
            seen[value] = true;
            assert_eq!(expected[value], entries.slots[index].index());
        }
        kani::cover!(true, "valid initial mapping and legal insertion index");
    }

    fn remove_preserves_ownership<S: SlotId>() {
        let (mut entries, mut slots) = entries::<S>();
        let expected = [
            entries.slots[0].index(),
            entries.slots[1].index(),
            entries.slots[2].index(),
        ];
        let index = usize::from(kani::any::<u8>());
        kani::assume(index < 3);
        let removed = entries.slots[index];
        let mut free = None;
        assert_eq!(entries.remove(&mut slots, &mut free, removed), index as u8);
        assert_eq!(entries.values.len(), 2);
        assert_eq!(free.unwrap().index(), removed.index());
        assert!(matches!(slots[removed.index()], Slot::Free(None)));
        assert_bijection(&entries, &slots);
        for (value, slot) in entries.values.iter().zip(&entries.slots) {
            assert_ne!(slot.index(), removed.index());
            assert_eq!(expected[usize::from(*value)], slot.index());
        }
        kani::cover!(true, "valid initial mapping and live removal index");
    }

    macro_rules! ownership_proof {
        ($name:ident, $width:ty, $operation:ident) => {
            #[kani::proof]
            #[kani::unwind(5)]
            #[kani::stub(fastrand::Rng::usize, arbitrary_index)]
            fn $name() {
                $operation::<$width>();
            }
        };
    }

    ownership_proof!(insert_u8, u8, insert_preserves_ownership);
    ownership_proof!(insert_u16, u16, insert_preserves_ownership);
    ownership_proof!(insert_u32, u32, insert_preserves_ownership);
    ownership_proof!(insert_usize, usize, insert_preserves_ownership);
    ownership_proof!(remove_u8, u8, remove_preserves_ownership);
    ownership_proof!(remove_u16, u16, remove_preserves_ownership);
    ownership_proof!(remove_u32, u32, remove_preserves_ownership);
    ownership_proof!(remove_usize, usize, remove_preserves_ownership);

    #[kani::proof]
    #[kani::unwind(5)]
    #[kani::stub(fastrand::Rng::usize, arbitrary_index)]
    fn shuffle_preserves_ownership() {
        let (entries, mut slots) = entries::<u8>();
        let expected = [entries.slots[0], entries.slots[1], entries.slots[2]];
        slots.pop();
        let first = entries.values[0];
        let registry = DenseRegistry {
            state: RefCell::new(State {
                entries: Some(entries),
                slots,
                free: None,
                pending: Vec::new(),
                random: seeded_rng(),
            }),
        };
        registry.shuffle_first();
        let state = registry.state.borrow();
        let entries = state.entries.as_ref().unwrap();
        assert_eq!(entries.values.len(), 3);
        assert_ne!(entries.values[0], first);
        assert_bijection(entries, &state.slots);
        for (value, slot) in entries.values.iter().zip(&entries.slots) {
            assert_eq!(expected[usize::from(*value)], *slot);
        }
    }

    /// Uses a valid two-member registry with spare capacity to isolate batch and
    /// handle semantics from the standard library's vector allocation machinery.
    fn two_members() -> DenseRegistry<u8> {
        let mut values = Vec::with_capacity(3);
        values.extend([0, 1]);
        let mut reverse = Vec::with_capacity(3);
        reverse.extend([0, 1]);
        let mut slots = Vec::with_capacity(3);
        slots.extend([Slot::Occupied(0), Slot::Occupied(1)]);
        DenseRegistry {
            state: RefCell::new(State {
                entries: Some(Entries {
                    values,
                    slots: reverse,
                }),
                slots,
                free: None,
                pending: Vec::with_capacity(2),
                random: seeded_rng(),
            }),
        }
    }

    /// Exercises the production removal and insertion transitions directly, leaving
    /// weak-handle bookkeeping to the lifetime harness and ordinary API tests.
    fn check_batch_slot_reuse(owner: u8) {
        let registry = two_members();
        let mut inserted = false;
        let mut visited = [false; 2];
        registry.for_each(|value| {
            assert!(!visited[usize::from(*value)]);
            visited[usize::from(*value)] = true;
            if !inserted {
                registry.remove(owner);
                let slot = registry.state.borrow_mut().insert(2).unwrap();
                assert_eq!(slot, 2, "slot was reused before the batch finished");
                inserted = true;
            }
        });
        assert!(visited.into_iter().all(|seen| seen));
        {
            let state = registry.state.borrow();
            assert!(state.pending.is_empty());
            assert_eq!(state.free.unwrap(), owner);
            assert!(matches!(state.slots[usize::from(owner)], Slot::Free(None)));
            assert_bijection(state.entries.as_ref().unwrap(), &state.slots);
        }
        let mut state = registry.state.borrow_mut();
        assert_eq!(state.insert(3).unwrap(), owner);
        let entries = state.entries.as_ref().unwrap();
        assert_eq!(entries.values.len(), 3);
        assert_bijection(entries, &state.slots);
        for (value, slot) in entries.values.iter().zip(&entries.slots) {
            assert_eq!(if *value == 3 { owner } else { *value }, *slot);
        }
        kani::cover!(true, "batch completes and released slot is reused");
    }

    #[kani::proof]
    #[kani::unwind(5)]
    #[kani::stub(fastrand::Rng::usize, arbitrary_index)]
    fn batch_does_not_reuse_the_current_slot() {
        check_batch_slot_reuse(0);
    }

    #[kani::proof]
    #[kani::unwind(5)]
    #[kani::stub(fastrand::Rng::usize, arbitrary_index)]
    fn batch_does_not_reuse_a_later_slot() {
        check_batch_slot_reuse(1);
    }

    #[kani::proof]
    #[kani::unwind(5)]
    #[kani::stub(fastrand::Rng::usize, arbitrary_index)]
    fn free_slot_registration_keeps_live_ids() {
        let mut state = two_members().state.into_inner();
        state.slots.push(Slot::Free(None));
        state.free = Some(2);
        assert_eq!(state.insert(2).unwrap(), 2);
        assert!(state.free.is_none());
        let entries = state.entries.as_ref().unwrap();
        assert_eq!(entries.values.len(), 3);
        assert_bijection(entries, &state.slots);
        for (value, slot) in entries.values.iter().zip(&entries.slots) {
            assert_eq!(*value, *slot);
        }
    }

    #[kani::proof]
    #[kani::unwind(5)]
    fn handles_can_outlive_the_registry() {
        let registry = Rc::new(two_members());
        let handle = Registration {
            registry: Rc::downgrade(&registry),
            slot: 0,
        };
        let weak = Rc::downgrade(&registry);
        drop(registry);
        assert!(weak.upgrade().is_none());
        drop(handle);
    }

    #[kani::proof]
    #[kani::unwind(2)]
    #[kani::stub(fastrand::Rng::new, seeded_rng)]
    fn default_registry_has_no_live_or_free_slots() {
        let registry = DenseRegistry::<u8>::default();
        let state = registry.state.borrow();
        assert!(state.free.is_none());
        assert!(state.pending.is_empty());
        assert!(state.slots.is_empty());
        let entries = state.entries.as_ref().unwrap();
        assert!(entries.values.is_empty());
        assert!(entries.slots.is_empty());
    }
}

#[cfg(test)]
mod tests {
    //! Tests for dense storage, stable registrations, and deferred mutations.

    use std::{
        cell::{Cell, RefCell},
        panic::{catch_unwind, AssertUnwindSafe},
        rc::Rc,
    };

    use super::{DenseRegistry, Slot, SlotId};

    fn values<S: SlotId>(registry: &DenseRegistry<usize, S>) -> Vec<usize> {
        let mut values = Vec::new();
        registry.for_each(|value| values.push(*value));
        values
    }

    fn assert_contents<S: SlotId>(registry: &DenseRegistry<usize, S>, mut expected: Vec<usize>) {
        let mut actual = values(registry);
        actual.sort_unstable();
        expected.sort_unstable();
        assert_eq!(actual, expected);
        let state = registry.state.borrow();
        let entries = state.entries.as_ref().unwrap();
        assert_eq!(entries.values.len(), entries.slots.len());
        for (index, &slot) in entries.slots.iter().enumerate() {
            assert!(
                matches!(state.slots[slot.index()], Slot::Occupied(found) if found.index() == index)
            );
        }
    }

    #[test]
    fn empty_and_mutable_iteration() {
        let registry = Rc::new(DenseRegistry::<_>::default());
        registry.for_each(|_: &mut usize| panic!("empty registry visited a value"));
        let registration = registry.insert(1).unwrap();
        registry.for_each(|value| *value += 1);
        assert_contents(&registry, vec![2]);
        drop(registration);
        assert_contents(&registry, vec![]);
    }

    #[test]
    fn insertion_uses_random_positions() {
        let registry = Rc::new(DenseRegistry::<_>::default());
        let mut random = fastrand::Rng::with_seed(123);
        registry.state.borrow_mut().random = fastrand::Rng::with_seed(123);
        let mut registrations = Vec::new();
        let mut expected = Vec::new();
        for value in 0..100 {
            let index = random.usize(..=expected.len());
            expected.push(value);
            expected.swap(index, value);
            registrations.push(registry.insert(value).unwrap());
            assert_eq!(values(&registry), expected);
        }
        assert_ne!(expected, (0..100).collect::<Vec<_>>());
    }

    #[test]
    fn handles_survive_swaps_and_slot_reuse() {
        let registry = Rc::new(DenseRegistry::<_>::default());
        let mut random = fastrand::Rng::with_seed(456);
        let mut registrations = Vec::new();
        let mut expected = Vec::new();
        for value in 0..1000 {
            registrations.push(registry.insert(value).unwrap());
            expected.push(value);
            if registrations.len() > 10 {
                let index = random.usize(..registrations.len());
                drop(registrations.swap_remove(index));
                expected.swap_remove(index);
            }
            assert_contents(&registry, expected.clone());
            registry.shuffle_first();
            assert_contents(&registry, expected.clone());
        }
        assert_eq!(registry.state.borrow().slots.len(), 11);
        while let Some(registration) = registrations.pop() {
            drop(registration);
            expected.pop();
            assert_contents(&registry, expected.clone());
        }
    }

    fn check_slot_width<S: SlotId>() {
        let registry = Rc::new(DenseRegistry::<usize, S>::default());
        let mut registrations = (0..128)
            .map(|value| registry.insert(value).unwrap())
            .collect::<Vec<_>>();
        let mut expected = (0..128).collect::<Vec<_>>();
        drop(registrations.swap_remove(42));
        expected.swap_remove(42);
        registry.shuffle_first();
        registrations.push(registry.insert(128).unwrap());
        expected.push(128);
        let original = values(&registry);
        let mut visited = Vec::new();
        let mut retained = None;
        registry.for_each(|value| {
            if visited.is_empty() {
                drop(registrations.pop());
                retained = Some(registry.insert(129).unwrap());
            }
            visited.push(*value);
        });
        assert_eq!(visited, original);
        expected.pop();
        expected.push(129);
        assert_contents(&registry, expected);
        assert_eq!(registry.state.borrow().slots.len(), 129);
        drop((registrations, retained));
        assert_contents(&registry, vec![]);
    }

    #[test]
    fn configurable_slot_widths_preserve_registry_semantics() {
        check_slot_width::<u8>();
        check_slot_width::<u16>();
        check_slot_width::<u32>();
        check_slot_width::<usize>();
    }

    #[test]
    fn u8_rejects_the_257th_insertion_and_reuses_released_slots() {
        let registry = Rc::new(DenseRegistry::<usize, u8>::default());
        let mut registrations = (0..=usize::from(u8::MAX))
            .map(|value| registry.insert(value).unwrap())
            .collect::<Vec<_>>();
        let original = values(&registry);
        let seed = registry.state.borrow().random.get_seed();
        assert_eq!(registry.insert(256).err(), Some(256));
        assert_eq!(values(&registry), original);
        assert_eq!(registry.state.borrow().random.get_seed(), seed);
        assert_eq!(registry.state.borrow().slots.len(), 256);
        let removed = registrations.swap_remove(42);
        let slot = removed.slot;
        drop(removed);
        let replacement = registry.insert(256).unwrap();
        assert_eq!(replacement.slot, slot);
        registry.shuffle_first();
        assert_contents(&registry, (0..=256).filter(|&value| value != 42).collect());
        drop((registrations, replacement));
        assert_contents(&registry, vec![]);
    }

    #[test]
    fn deferred_insertion_reserves_narrow_slot_ids_before_batch_end() {
        let registry = Rc::new(DenseRegistry::<usize, u8>::default());
        let registrations = (0..255)
            .map(|value| registry.insert(value).unwrap())
            .collect::<Vec<_>>();
        let mut retained = None;
        let original = values(&registry);
        let mut visited = Vec::new();
        registry.for_each(|value| {
            if retained.is_none() {
                retained = Some(registry.insert(255).unwrap());
                assert_eq!(registry.insert(256).err(), Some(256));
            }
            visited.push(*value);
        });
        assert_eq!(visited, original);
        assert_contents(&registry, (0..256).collect());
        drop((registrations, retained));
        assert_contents(&registry, vec![]);
    }

    #[test]
    fn deferred_removals_release_narrow_slot_ids_at_batch_end() {
        let registry = Rc::new(DenseRegistry::<usize, u8>::default());
        let mut registrations = (0..256)
            .map(|value| registry.insert(value).unwrap())
            .collect::<Vec<_>>();
        let original = values(&registry);
        let mut visited = Vec::new();
        registry.for_each(|value| {
            if visited.is_empty() {
                drop(registrations.pop());
                assert_eq!(registry.insert(256).err(), Some(256));
            }
            visited.push(*value);
        });
        assert_eq!(visited, original);
        assert_contents(&registry, (0..255).collect());
        let replacement = registry.insert(256).unwrap();
        assert_eq!(replacement.slot, 255);
        assert_eq!(registry.state.borrow().slots.len(), 256);
        drop((registrations, replacement));
        assert_contents(&registry, vec![]);
    }

    #[test]
    fn deferred_insertion_can_reuse_previously_free_narrow_slots() {
        let registry = Rc::new(DenseRegistry::<usize, u8>::default());
        let mut registrations = (0..256)
            .map(|value| registry.insert(value).unwrap())
            .collect::<Vec<_>>();
        drop(registrations.pop());
        let mut retained = None;
        registry.for_each(|_| {
            if retained.is_none() {
                retained = Some(registry.insert(256).unwrap());
            }
        });
        assert_eq!(retained.as_ref().unwrap().slot, 255);
        assert_contents(&registry, (0..255).chain([256]).collect());
        drop((registrations, retained));
        assert_contents(&registry, vec![]);
    }

    #[test]
    fn registry_random_streams_are_independent() {
        let first = Rc::new(DenseRegistry::<usize, u8>::default());
        let second = Rc::new(DenseRegistry::<usize, u8>::default());
        let unrelated = Rc::new(DenseRegistry::<usize, u8>::default());
        first.state.borrow_mut().random = fastrand::Rng::with_seed(1234);
        second.state.borrow_mut().random = fastrand::Rng::with_seed(1234);
        let mut registrations = Vec::new();
        for value in 0..100 {
            registrations.push(first.insert(value).unwrap());
            let temporary = unrelated.insert(value).unwrap();
            unrelated.shuffle_first();
            registrations.push(second.insert(value).unwrap());
            assert_eq!(values(&first), values(&second));
            first.shuffle_first();
            drop(temporary);
            second.shuffle_first();
            assert_eq!(values(&first), values(&second));
        }
        for registry in [&first, &second] {
            let mut inserted = false;
            registry.for_each(|_| {
                if !inserted {
                    inserted = true;
                    for value in 100..110 {
                        registrations.push(registry.insert(value).unwrap());
                    }
                }
            });
        }
        assert_eq!(values(&first), values(&second));
        assert_eq!(
            first.state.borrow().random.get_seed(),
            second.state.borrow().random.get_seed(),
        );
    }

    /// Removes and replaces two registrations in every possible sequential
    /// ordering, both immediately and within a batch, using fixed RNG seeds.
    #[test]
    fn live_and_deferred_slot_ids_are_not_reused() {
        let orders = [
            [0, 0, 1, 1],
            [0, 1, 0, 1],
            [0, 1, 1, 0],
            [1, 0, 0, 1],
            [1, 0, 1, 0],
            [1, 1, 0, 0],
        ];
        for (deferred, seed, order) in [false, true].into_iter().flat_map(|deferred| {
            (0..4)
                .flat_map(move |seed| orders.into_iter().map(move |order| (deferred, seed, order)))
        }) {
            let registry = Rc::new(DenseRegistry::<usize, u8>::default());
            registry.state.borrow_mut().random = fastrand::Rng::with_seed(seed);
            let pinned = [registry.insert(0).unwrap(), registry.insert(1).unwrap()];
            let mut handles = [
                Some(registry.insert(2).unwrap()),
                Some(registry.insert(3).unwrap()),
            ];
            let original_slots = handles
                .each_ref()
                .map(|handle| handle.as_ref().unwrap().slot);
            let original_values = values(&registry);
            let mut expected_values = [Some(2), Some(3)];
            let mut process_requests = || {
                for actor in order {
                    if handles[actor].is_none() {
                        let handle = registry.insert(10 + actor).unwrap();
                        for live in pinned.iter().chain(handles.iter().flatten()) {
                            assert_ne!(handle.slot, live.slot, "reused a live slot ID");
                        }
                        if deferred {
                            assert!(
                                !original_slots.contains(&handle.slot),
                                "reused a slot before its batch finished",
                            );
                        }
                        handles[actor] = Some(handle);
                        expected_values[actor] = Some(10 + actor);
                    } else {
                        drop(handles[actor].take().unwrap());
                        expected_values[actor] = None;
                    }
                    if !deferred {
                        registry.shuffle_first();
                        let mut expected = vec![0, 1];
                        for (actor, handle) in handles.iter().enumerate() {
                            let Some(handle) = handle else { continue };
                            let state = registry.state.borrow();
                            let Slot::Occupied(index) = state.slots[handle.slot.index()] else {
                                panic!("live handle lost its occupied slot");
                            };
                            assert_eq!(
                                state.entries.as_ref().unwrap().values[index.index()],
                                expected_values[actor].unwrap(),
                            );
                        }
                        expected.extend(expected_values.into_iter().flatten());
                        assert_contents(&registry, expected);
                    }
                }
            };
            if deferred {
                let mut visited = Vec::new();
                registry.for_each(|value| {
                    if visited.is_empty() {
                        process_requests();
                    }
                    visited.push(*value);
                });
                assert_eq!(visited, original_values);
            } else {
                process_requests();
            }
            assert_contents(&registry, vec![0, 1, 10, 11]);
            if deferred {
                let reused = [registry.insert(20).unwrap(), registry.insert(21).unwrap()];
                let mut actual = reused.each_ref().map(|handle| handle.slot);
                let mut expected = original_slots;
                actual.sort_unstable();
                expected.sort_unstable();
                assert_eq!(actual, expected, "unregistered slots were not reusable");
                drop(reused);
            }
            drop((handles, pinned));
            assert_contents(&registry, vec![]);
        }
    }

    #[test]
    fn mutations_wait_for_the_entire_batch() {
        let registry = Rc::new(DenseRegistry::<_>::default());
        let mut registrations = (0..10)
            .map(|value| registry.insert(value).unwrap())
            .collect::<Vec<_>>();
        let original = values(&registry);
        let mut visited = Vec::new();
        let mut retained = None;
        registry.for_each(|value| {
            if visited.is_empty() {
                registrations.clear();
                retained = Some(registry.insert(10).unwrap());
                drop(registry.insert(11).unwrap());
            }
            visited.push(*value);
        });
        assert_eq!(visited, original);
        assert_contents(&registry, vec![10]);
        drop(retained);
        assert_contents(&registry, vec![]);
    }

    #[test]
    fn unwinding_applies_deferred_mutations_and_restores_iteration() {
        let registry = Rc::new(DenseRegistry::<_>::default());
        let mut registrations = (0..3)
            .map(|value| registry.insert(value).unwrap())
            .collect::<Vec<_>>();
        let mut retained = None;
        let result = catch_unwind(AssertUnwindSafe(|| {
            registry.for_each(|_| {
                registrations.clear();
                retained = Some(registry.insert(10).unwrap());
                panic!("visitor failed");
            });
        }));
        assert!(result.is_err());
        assert_contents(&registry, vec![10]);
        drop(retained);
        assert_contents(&registry, vec![]);
    }

    #[test]
    fn recursive_iteration_panics_without_losing_the_batch() {
        let registry = Rc::new(DenseRegistry::<_>::default());
        let registration = registry.insert(1).unwrap();
        let result = catch_unwind(AssertUnwindSafe(|| {
            registry.for_each(|_| registry.for_each(|_| {}));
        }));
        assert!(result.is_err());
        assert_contents(&registry, vec![1]);
        drop(registration);
    }

    #[test]
    fn shuffle_first_handles_small_registries() {
        let registry = Rc::new(DenseRegistry::<_>::default());
        registry.shuffle_first();
        assert_contents(&registry, vec![]);
        let first = registry.insert(1).unwrap();
        registry.shuffle_first();
        assert_contents(&registry, vec![1]);
        let second = registry.insert(2).unwrap();
        let before = values(&registry);
        registry.shuffle_first();
        assert_eq!(values(&registry), vec![before[1], before[0]]);
        drop(first);
        assert_contents(&registry, vec![2]);
        drop(second);
        assert_contents(&registry, vec![]);
    }

    #[test]
    fn shuffle_first_swaps_with_a_random_different_position() {
        let registry = Rc::new(DenseRegistry::<_>::default());
        let registrations = (0..10)
            .map(|value| registry.insert(value).unwrap())
            .collect::<Vec<_>>();
        let mut random = fastrand::Rng::with_seed(789);
        registry.state.borrow_mut().random = fastrand::Rng::with_seed(789);
        for _ in 0..100 {
            let mut expected = values(&registry);
            let first = expected[0];
            let other = random.usize(1..expected.len());
            expected.swap(0, other);
            registry.shuffle_first();
            assert_eq!(values(&registry), expected);
            assert_ne!(expected[0], first);
            assert_contents(&registry, (0..10).collect());
        }
        drop(registrations);
        assert_contents(&registry, vec![]);
    }

    #[test]
    fn shuffling_during_iteration_panics_and_restores_the_batch() {
        let registry = Rc::new(DenseRegistry::<_>::default());
        let registration = registry.insert(1).unwrap();
        let result = catch_unwind(AssertUnwindSafe(|| {
            registry.for_each(|_| registry.shuffle_first());
        }));
        assert!(result.is_err());
        assert_contents(&registry, vec![1]);
        drop(registration);
    }

    struct OnDrop(Option<Box<dyn FnOnce()>>);

    impl Drop for OnDrop {
        fn drop(&mut self) {
            if let Some(callback) = self.0.take() {
                callback();
            }
        }
    }

    #[test]
    fn value_destructors_can_mutate_the_registry() {
        for during_batch in [false, true] {
            let registry = Rc::new(DenseRegistry::<_>::default());
            let other = Rc::new(RefCell::new(Some(
                registry
                    .insert(OnDrop(None))
                    .ok()
                    .expect("registry unexpectedly full"),
            )));
            let added = Rc::new(RefCell::new(None));
            let weak = Rc::downgrade(&registry);
            let registration = registry
                .insert(OnDrop(Some(Box::new({
                    let other = other.clone();
                    let added = added.clone();
                    move || {
                        drop(other.borrow_mut().take());
                        *added.borrow_mut() = Some(
                            weak.upgrade()
                                .unwrap()
                                .insert(OnDrop(None))
                                .ok()
                                .expect("registry unexpectedly full"),
                        );
                    }
                }))))
                .ok()
                .expect("registry unexpectedly full");
            if during_batch {
                let mut registration = Some(registration);
                registry.for_each(|_| drop(registration.take()));
            } else {
                drop(registration);
            }
            let mut count = 0;
            registry.for_each(|_| count += 1);
            assert_eq!(count, 1);
            drop(added.borrow_mut().take());
            registry.for_each(|_| panic!("registry should be empty"));
        }
    }

    #[test]
    fn handles_do_not_keep_the_registry_or_values_alive() {
        let registry = Rc::new(DenseRegistry::<_>::default());
        let dropped = Rc::new(Cell::new(false));
        let registration = registry
            .insert(OnDrop(Some(Box::new({
                let dropped = dropped.clone();
                move || dropped.set(true)
            }))))
            .ok()
            .expect("registry unexpectedly full");
        drop(registry);
        assert!(dropped.get());
        drop(registration);
    }

    #[test]
    fn capacity_error_returns_the_value_without_dropping_it() {
        let registry = Rc::new(DenseRegistry::<OnDrop, u8>::default());
        let registrations = (0..256)
            .map(|_| {
                registry
                    .insert(OnDrop(None))
                    .ok()
                    .expect("registry unexpectedly full")
            })
            .collect::<Vec<_>>();
        let dropped = Rc::new(Cell::new(false));
        let weak = Rc::downgrade(&registry);
        let rejected = registry
            .insert(OnDrop(Some(Box::new({
                let dropped = dropped.clone();
                move || {
                    let mut count = 0;
                    weak.upgrade().unwrap().for_each(|_| count += 1);
                    assert_eq!(count, 256);
                    dropped.set(true);
                }
            }))))
            .err()
            .expect("full registry accepted another value");
        assert!(!dropped.get());
        drop(rejected);
        assert!(dropped.get());
        drop(registrations);
    }
}
