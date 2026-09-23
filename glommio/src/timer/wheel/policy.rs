// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience
//
//! What a wheel is allowed to decide for itself, and what the caller decides.

/// Where a deadline that falls between two ticks is placed.
///
/// The wheel's resolution is one tick, so a deadline of 1.7 ticks has to be
/// stored at a whole one. Which way it goes is not a detail: it decides
/// whether a timer can fire before the caller asked for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rounding {
    /// Never early, possibly late. A future woken before its deadline has to
    /// check the clock and arm again, so a runtime wants this one.
    Up,
    /// Lowest average error, early or late. For sampling and metrics, where
    /// the direction of the error matters less than its size.
    Nearest,
    /// Never late, possibly early. For enforcing a deadline, where overshoot
    /// is the failure and a spurious wake is not.
    Down,
}

/// How the wheel answers "what is due next" for a level it cannot read without
/// breaking it down.
///
/// A coarse slot holds deadlines spanning its whole resolution, and finding
/// the earliest means examining them. Whether that is worth doing depends
/// entirely on the caller's access pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoarseQuery {
    /// Answer from the slot's position alone: an entry in the slot at offset
    /// `k` from the cursor cannot be due before that bucket begins.
    ///
    /// Constant cost and never reads an entry, at the price of naming a time
    /// earlier than the truth, which costs the caller one wasted poll. Arming
    /// and cancelling stay flat because neither disturbs the answer.
    BucketBound,
    /// Track the earliest deadline in each slot and answer with it.
    ///
    /// Exact, so no wasted poll. The cost lands on cancellation: withdrawing
    /// the entry that supplied a slot's minimum invalidates it, and the next
    /// query rebuilds that slot. A workload that arms a timeout per operation
    /// and cancels it on completion pays this on every operation.
    ExactMinimum,
}

/// What to do with a deadline beyond the wheel's furthest slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overflow {
    /// Park it in an ordered map until the wheel's reach catches up. Costs a
    /// tree insertion for deadlines that are, by construction, rare.
    Park,
    /// Refuse the insertion. For callers who would rather hear about it than
    /// discover a timer fired hours early.
    Reject,
}

/// A wheel's shape and the decisions it makes.
///
/// Implemented as associated constants so a layout is fixed per wheel type and
/// costs nothing to consult.
pub trait Policy {
    /// Slots per level, coarsest last. Every entry must be a power of two.
    const SLOTS: &'static [usize];
    /// Ticks spanned by one slot of each level. The first is always 1, and
    /// every entry must be a power of two.
    const RESOLUTION: &'static [u64];

    /// Which way a deadline between two ticks is moved.
    const ROUNDING: Rounding = Rounding::Up;
    /// How much work answering "what is due next" is worth at a coarse level.
    const COARSE_QUERY: CoarseQuery = CoarseQuery::BucketBound;
    /// What happens to a deadline past the wheel's furthest slot.
    const OVERFLOW: Overflow = Overflow::Park;
}

/// Milliseconds, four levels, eighteen hours of reach, and an entry that never
/// expires before it was asked to.
///
/// The shape an event loop wants, and the one every figure in this crate's
/// benchmarks was measured against. `BucketBound` in particular answers a
/// workload that arms a timeout per operation and withdraws it on completion,
/// where tracking exact minima turns every withdrawal into a slot rebuild.
#[derive(Debug, Clone, Copy)]
pub struct Millis;

impl Policy for Millis {
    const SLOTS: &'static [usize] = &[256, 64, 64, 64];
    const RESOLUTION: &'static [u64] = &[1, 256, 16_384, 1_048_576];
}

/// Checked once when a wheel is built, so a layout that would be silently slow
/// or silently wrong cannot produce a wheel at all.
pub(crate) fn validate<P: Policy>() {
    assert_eq!(
        P::SLOTS.len(),
        P::RESOLUTION.len(),
        "a level needs both a slot count and a resolution"
    );
    assert!(!P::SLOTS.is_empty(), "a wheel needs at least one level");
    assert_eq!(P::RESOLUTION[0], 1, "the finest level spans one tick");

    for (level, (&slots, &resolution)) in P::SLOTS.iter().zip(P::RESOLUTION).enumerate() {
        // Powers of two are not a style preference. The hot path divides by a
        // resolution and takes a remainder by a slot count on every insertion;
        // as powers of two those are a shift and a mask, and the layout can
        // then be a runtime value without costing anything against constants.
        assert!(
            slots.is_power_of_two(),
            "level {level} has {slots} slots, which is not a power of two"
        );
        assert!(
            resolution.is_power_of_two(),
            "level {level} spans {resolution} ticks, which is not a power of two"
        );
        if level > 0 {
            // Each level must span exactly what the one below it covers, or
            // cascading either loses deadlines or visits them twice.
            let below = P::RESOLUTION[level - 1] * P::SLOTS[level - 1] as u64;
            assert_eq!(
                resolution, below,
                "level {level} spans {resolution} ticks but the level below covers {below}"
            );
        }
    }
}
