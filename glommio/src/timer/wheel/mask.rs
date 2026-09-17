// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience
//
//! Which slots of a level hold anything.
//!
//! Finding the next occupied slot is on the path a caller takes before every
//! sleep, so it is searched a word at a time rather than a bit at a time: an
//! empty 256-slot level answers in four iterations instead of 256.

#[derive(Debug, Clone)]
pub(crate) struct SlotMask {
    words: Box<[u64]>,
    slots: usize,
}

impl SlotMask {
    pub(crate) fn new(slots: usize) -> Self {
        debug_assert!(slots.is_power_of_two());
        Self {
            words: vec![0; slots.div_ceil(64)].into_boxed_slice(),
            slots,
        }
    }

    pub(crate) fn set(&mut self, slot: usize) {
        self.words[slot / 64] |= 1 << (slot % 64);
    }

    pub(crate) fn clear(&mut self, slot: usize) {
        self.words[slot / 64] &= !(1 << (slot % 64));
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.words.iter().all(|word| *word == 0)
    }

    /// How many slots ahead of `from` the first occupied one lies, wrapping
    /// once. `None` when the level is empty.
    pub(crate) fn distance_to_next(&self, from: usize) -> Option<usize> {
        debug_assert!(from < self.slots);
        let words = self.words.len();
        let first = from / 64;
        let bit = from % 64;

        // One pass over the words from `from`, then the starting word once
        // more, which is where a search that wrapped ends up. That last visit
        // needs no mask of its own: reaching it means the first visit found
        // nothing at or above `from`, so only the bits below it can be set.
        for step in 0..=words {
            let index = (first + step) % words;
            let mut word = self.words[index];
            if step == 0 {
                word &= u64::MAX << bit;
            }
            if word != 0 {
                let slot = index * 64 + word.trailing_zeros() as usize;
                return Some((slot + self.slots - from) % self.slots);
            }
        }
        None
    }
}
