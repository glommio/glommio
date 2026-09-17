// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience
//
//! The contract, over every combination of layout and policy.
//!
//! A policy is a set of associated constants, so each combination is a
//! distinct type. Writing them out would be hundreds of declarations, so they
//! are composed instead: one marker type per choice on each axis, and a
//! `Combo` that implements `Policy` from four of them. Adding a rounding mode
//! then costs one type and one line of the matrix rather than a pass over
//! every layout.

use super::{CoarseQuery, Key, Overflow, Policy, Rounding, TimingWheel};
use std::{
    marker::PhantomData,
    time::{Duration, Instant},
};

// ---- the axes -----------------------------------------------------------

trait Layout {
    const NAME: &'static str;
    const SLOTS: &'static [usize];
    const RESOLUTION: &'static [u64];
}

trait Round {
    const NAME: &'static str;
    const VALUE: Rounding;
}

trait Query {
    const NAME: &'static str;
    const VALUE: CoarseQuery;
}

trait Spill {
    const NAME: &'static str;
    const VALUE: Overflow;
}

macro_rules! layouts {
    ($($name:ident: $slots:expr, $res:expr;)*) => {$(
        struct $name;
        impl Layout for $name {
            const NAME: &'static str = stringify!($name);
            const SLOTS: &'static [usize] = &$slots;
            const RESOLUTION: &'static [u64] = &$res;
        }
    )*};
}

layouts! {
    // One level: no cascading at all, everything either fits or overflows.
    Flat:        [64], [1];
    // Two levels, the smallest shape that cascades.
    Tiny:        [2, 2], [1, 2];
    // Deep and narrow: eight levels of two slots, so an entry cascades seven
    // times on its way in. Maximises the moves a handle has to survive.
    Deep:        [2, 2, 2, 2, 2, 2, 2, 2], [1, 2, 4, 8, 16, 32, 64, 128];
    // Wide and shallow: one cascade, but a thousand slots to search.
    Wide:        [1024, 1024], [1, 1024];
    // Uneven, because equal levels can hide an arithmetic mistake that only
    // shows when the levels differ.
    Uneven:      [256, 8, 64, 4], [1, 256, 2048, 131_072];
    // What a reactor uses.
    Realistic:   [256, 64, 64, 64], [1, 256, 16_384, 1_048_576];
}

macro_rules! choices {
    ($trait:ident, $ty:ty, $($name:ident => $value:expr;)*) => {$(
        struct $name;
        impl $trait for $name {
            const NAME: &'static str = stringify!($name);
            const VALUE: $ty = $value;
        }
    )*};
}

choices!(Round, Rounding,
    Up => Rounding::Up;
    Down => Rounding::Down;
    Nearest => Rounding::Nearest;
);

choices!(Query, CoarseQuery,
    Bound => CoarseQuery::BucketBound;
    Exact => CoarseQuery::ExactMinimum;
);

choices!(Spill, Overflow,
    Park => Overflow::Park;
    Reject => Overflow::Reject;
);

/// One point in the matrix.
struct Combo<L, R, Q, S>(PhantomData<(L, R, Q, S)>);

impl<L: Layout, R: Round, Q: Query, S: Spill> Policy for Combo<L, R, Q, S> {
    const SLOTS: &'static [usize] = L::SLOTS;
    const RESOLUTION: &'static [u64] = L::RESOLUTION;
    const ROUNDING: Rounding = R::VALUE;
    const COARSE_QUERY: CoarseQuery = Q::VALUE;
    const OVERFLOW: Overflow = S::VALUE;
}

fn name<L: Layout, R: Round, Q: Query, S: Spill>() -> String {
    format!("{}/{}/{}/{}", L::NAME, R::NAME, Q::NAME, S::NAME)
}

// ---- the property -------------------------------------------------------

fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

fn exercise<L: Layout, R: Round, Q: Query, S: Spill>(seeds: u64) {
    let label = name::<L, R, Q, S>();
    type P<L, R, Q, S> = Combo<L, R, Q, S>;

    // A wheel two slots deep reaches four ticks; one 256 wide reaches hours.
    // Draw deadlines against the wheel's own reach so every layout is
    // exercised across its whole range instead of overflowing immediately.
    let reach = L::RESOLUTION[L::RESOLUTION.len() - 1] * L::SLOTS[L::SLOTS.len() - 1] as u64;

    for seed in 1..=seeds {
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let start = Instant::now();
        let mut wheel: TimingWheel<u64, P<L, R, Q, S>> = TimingWheel::starting_at(start);
        let mut live: Vec<(Key, Instant)> = Vec::new();
        let mut now = start;

        for step in 0..200 {
            match xorshift(&mut rng) % 10 {
                0..=4 => {
                    // Inside the wheel, around its edge, and past it.
                    let micros = match xorshift(&mut rng) % 4 {
                        0 => xorshift(&mut rng) % 1_500,
                        1 => xorshift(&mut rng) % (reach * 1_000).max(2),
                        2 => (reach * 1_000).saturating_sub(xorshift(&mut rng) % 2_000),
                        _ => xorshift(&mut rng) % (reach * 4_000).max(2),
                    };
                    let deadline = now + Duration::from_micros(micros);
                    if let Some(key) = wheel.try_insert(deadline, micros) {
                        live.push((key, deadline));
                    }
                }
                5..=6 if !live.is_empty() => {
                    let i = (xorshift(&mut rng) % live.len() as u64) as usize;
                    let (key, _) = live.swap_remove(i);
                    wheel.remove(key);
                }
                _ => {
                    let step_us = match xorshift(&mut rng) % 3 {
                        0 => xorshift(&mut rng) % 900,
                        1 => xorshift(&mut rng) % (reach * 200).max(2),
                        _ => xorshift(&mut rng) % (reach * 2_000).max(2),
                    };
                    now += Duration::from_micros(step_us);
                    wheel.advance_to(now);

                    let drained: Vec<_> = wheel.drain_expired().collect();
                    if let Some(next) = wheel.next_expiry() {
                        assert!(
                            next > now,
                            "{label} seed {seed} step {step}: next_expiry is {:?} in the PAST",
                            now.duration_since(next)
                        );
                    }
                    let allowed_early = match R::VALUE {
                        Rounding::Up => Duration::ZERO,
                        Rounding::Down | Rounding::Nearest => Duration::from_millis(1),
                    };
                    for (key, _) in drained {
                        if let Some(pos) = live.iter().position(|(k, _)| *k == key) {
                            let (_, deadline) = live.swap_remove(pos);
                            assert!(
                                deadline <= now + allowed_early,
                                "{label} seed {seed} step {step}: expired {:?} EARLY",
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
                    "{label} seed {seed} step {step}: next_expiry is {:?} LATER than a live \
                     deadline",
                    next.duration_since(truth)
                );
            }
        }

        // Everything still held is accounted for, and nothing leaked.
        assert_eq!(
            wheel.len(),
            live.len(),
            "{label} seed {seed}: wheel holds {} entries, {} are live",
            wheel.len(),
            live.len()
        );
    }
}

/// Every policy combination, against one layout.
macro_rules! all_policies {
    ($layout:ty, $seeds:expr) => {
        exercise::<$layout, Up, Bound, Park>($seeds);
        exercise::<$layout, Up, Bound, Reject>($seeds);
        exercise::<$layout, Up, Exact, Park>($seeds);
        exercise::<$layout, Up, Exact, Reject>($seeds);
        exercise::<$layout, Down, Bound, Park>($seeds);
        exercise::<$layout, Down, Bound, Reject>($seeds);
        exercise::<$layout, Down, Exact, Park>($seeds);
        exercise::<$layout, Down, Exact, Reject>($seeds);
        exercise::<$layout, Nearest, Bound, Park>($seeds);
        exercise::<$layout, Nearest, Bound, Reject>($seeds);
        exercise::<$layout, Nearest, Exact, Park>($seeds);
        exercise::<$layout, Nearest, Exact, Reject>($seeds);
    };
}

const SEEDS: u64 = 40;

#[test]
fn one_level_holds_every_policy() {
    all_policies!(Flat, SEEDS);
}

#[test]
fn two_levels_hold_every_policy() {
    all_policies!(Tiny, SEEDS);
}

#[test]
fn eight_narrow_levels_hold_every_policy() {
    all_policies!(Deep, SEEDS);
}

#[test]
fn two_wide_levels_hold_every_policy() {
    all_policies!(Wide, SEEDS);
}

#[test]
fn uneven_levels_hold_every_policy() {
    all_policies!(Uneven, SEEDS);
}

#[test]
fn a_realistic_layout_holds_every_policy() {
    all_policies!(Realistic, SEEDS);
}
