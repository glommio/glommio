// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience
//
//! A hierarchical timing wheel with stable handles.
//!
//! The reactor keeps its timers here. The module is public only when
//! benchmarking, the way [`crate::nop`] is, so the wheel benchmarks can reach
//! it without widening the crate's API.
//!
//! Entries live in a slab; the wheel's slots hold indices into it. Cascading
//! therefore moves four-byte indices between slots rather than whole entries,
//! and a handle stays valid for an entry's whole life however many times it
//! moves, which is what makes cancellation an array index rather than a
//! lookup.
//!
//! The wheel never reads the clock. [`TimingWheel::advance_to`] is told what
//! time it is, so the whole structure is testable without waiting and a caller
//! with its own time source can drive it.
//!
//! What the wheel decides for itself and what the caller decides is spelled out
//! in [`Policy`]: the level layout, which way a deadline between two ticks is
//! rounded, and how much work answering "what is due next" is worth.

#![warn(missing_docs, missing_debug_implementations)]
// The module is `pub` only under the `bench` feature. A default build reaches
// it from the reactor alone, which wants a fraction of the surface, so the rest
// is unused there by construction rather than by neglect.
#![cfg_attr(not(feature = "bench"), allow(dead_code, unused_imports))]

#[cfg(test)]
mod contract_tests;
mod mask;
#[cfg(test)]
mod matrix_tests;
mod policy;
mod slab;
mod timing;

pub use policy::{CoarseQuery, Millis, Overflow, Policy, Rounding};
pub use slab::Key;
pub use timing::TimingWheel;

/// The examples the standalone crate carried in its README.
///
/// They ran as doc tests there. Here the module is private outside the `bench`
/// feature, so doc tests on it would not run in a default-feature build, and
/// CI runs `cargo test --doc` without features. As ordinary tests they run
/// every time.
#[cfg(test)]
mod readme_examples {
    use super::{Millis, Policy, Rounding, TimingWheel};
    use std::time::{Duration, Instant};

    #[test]
    fn insert_then_withdraw_before_it_fires() {
        let start = Instant::now();
        let mut wheel: TimingWheel<&str, Millis> = TimingWheel::starting_at(start);

        let key = wheel.insert(start + Duration::from_millis(50), "read timeout");

        // Nothing is due yet, and the wheel says when to come back.
        wheel.advance_to(start + Duration::from_millis(10));
        let mut due = Vec::new();
        wheel.drain_expired_into(&mut due);
        assert!(due.is_empty());
        assert!(wheel.next_expiry().unwrap() <= start + Duration::from_millis(50));

        // The read completed, so withdraw the timeout before it fires.
        assert_eq!(wheel.remove(key), Some("read timeout"));
        assert!(wheel.is_empty());
    }

    struct Coarse;
    impl Policy for Coarse {
        const SLOTS: &'static [usize] = &[64, 64, 64];
        const RESOLUTION: &'static [u64] = &[1, 64, 4_096];
        const ROUNDING: Rounding = Rounding::Down;
    }

    #[test]
    fn building_a_wheel_checks_the_layout_tiles() {
        // Each level here spans exactly what the one below it covers,
        // 64 x 1 and then 64 x 64.
        let wheel: TimingWheel<(), Coarse> = TimingWheel::new();
        assert!(wheel.is_empty());
    }
}
