// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience
//
//! The wheel's contract, held under randomized traffic.
//!
//! Two halves, and only one is obvious. Under `Rounding::Up` an entry may
//! expire late and never early. And `next_expiry` may name a time earlier than
//! the truth and never a later one, because a caller sleeps on that answer
//! with no clock behind it: an answer one tick late is a deadline missed.

use super::{CoarseQuery, Key, Millis, Overflow, Policy, Rounding, TimingWheel};
use std::time::{Duration, Instant};

struct Exact;
impl Policy for Exact {
    const SLOTS: &'static [usize] = Millis::SLOTS;
    const RESOLUTION: &'static [u64] = Millis::RESOLUTION;
    const COARSE_QUERY: CoarseQuery = CoarseQuery::ExactMinimum;
}

struct NeverLate;
impl Policy for NeverLate {
    const SLOTS: &'static [usize] = Millis::SLOTS;
    const RESOLUTION: &'static [u64] = Millis::RESOLUTION;
    const ROUNDING: Rounding = Rounding::Down;
}

struct Closest;
impl Policy for Closest {
    const SLOTS: &'static [usize] = Millis::SLOTS;
    const RESOLUTION: &'static [u64] = Millis::RESOLUTION;
    const ROUNDING: Rounding = Rounding::Nearest;
}

struct Bounded;
impl Policy for Bounded {
    const SLOTS: &'static [usize] = Millis::SLOTS;
    const RESOLUTION: &'static [u64] = Millis::RESOLUTION;
    const OVERFLOW: Overflow = Overflow::Reject;
}

struct TwoLevel;
impl Policy for TwoLevel {
    const SLOTS: &'static [usize] = &[64, 64];
    const RESOLUTION: &'static [u64] = &[1, 64];
}

fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

fn contract_holds<P: Policy>(name: &str, seeds: u64) {
    for seed in 1..=seeds {
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let start = Instant::now();
        let mut wheel: TimingWheel<u64, P> = TimingWheel::starting_at(start);
        let mut live: Vec<(Key, Instant)> = Vec::new();
        let mut now = start;

        for step in 0..400 {
            match xorshift(&mut rng) % 10 {
                0..=4 => {
                    // Three bands: one modulus cannot reach both a millisecond
                    // and the far end of the wheel.
                    let micros = match xorshift(&mut rng) % 3 {
                        0 => xorshift(&mut rng) % 4_000,
                        1 => xorshift(&mut rng) % 90_000_000,
                        _ => xorshift(&mut rng) % 200_000_000_000,
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
                        1 => xorshift(&mut rng) % 50_000,
                        _ => xorshift(&mut rng) % 3_000_000,
                    };
                    now += Duration::from_micros(step_us);
                    wheel.advance_to(now);

                    let drained: Vec<_> = wheel.drain_expired().collect();
                    if let Some(next) = wheel.next_expiry() {
                        assert!(
                            next > now,
                            "{name} seed {seed} step {step}: next_expiry is {:?} in the PAST",
                            now.duration_since(next)
                        );
                    }
                    for (key, _) in drained {
                        if let Some(pos) = live.iter().position(|(k, _)| *k == key) {
                            let (_, deadline) = live.swap_remove(pos);
                            // How early an entry may expire is exactly what
                            // the rounding policy decides, so each one gets
                            // the bound it promises rather than a shared one.
                            let allowed_early = match P::ROUNDING {
                                Rounding::Up => Duration::ZERO,
                                // A deadline moves down to the tick below it,
                                // so at most one whole tick early.
                                Rounding::Down => Duration::from_millis(1),
                                // ...and to the nearer of the two, so at most
                                // half a tick, but a tick is a safe bound.
                                Rounding::Nearest => Duration::from_millis(1),
                            };
                            assert!(
                                deadline <= now + allowed_early,
                                "{name} seed {seed} step {step}: expired {:?} EARLY, \
                                 more than {allowed_early:?}",
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
                    "{name} seed {seed} step {step}: next_expiry is {:?} LATER than a live deadline",
                    next.duration_since(truth)
                );
            }
        }
    }
}

#[test]
fn reactor_policy_holds_its_contract() {
    contract_holds::<Millis>("Millis", 200);
}

#[test]
fn exact_minimum_holds_the_same_contract() {
    contract_holds::<Exact>("ExactMinimum", 200);
}

#[test]
fn a_two_level_wheel_holds_it_too() {
    contract_holds::<TwoLevel>("two-level", 200);
}

#[test]
fn rounding_down_never_expires_more_than_a_tick_early() {
    contract_holds::<NeverLate>("Rounding::Down", 200);
}

#[test]
fn rounding_to_the_nearest_tick_holds_the_contract() {
    contract_holds::<Closest>("Rounding::Nearest", 200);
}

#[test]
fn rejecting_overflow_refuses_what_it_cannot_hold_and_nothing_else() {
    let start = Instant::now();
    let mut wheel: TimingWheel<u64, Bounded> = TimingWheel::starting_at(start);

    // The wheel's reach is one revolution of its coarsest level.
    let reach = Duration::from_millis(
        Bounded::RESOLUTION[Bounded::RESOLUTION.len() - 1]
            * Bounded::SLOTS[Bounded::SLOTS.len() - 1] as u64,
    );

    assert!(
        wheel
            .try_insert(start + reach - Duration::from_millis(1), 1)
            .is_some(),
        "a deadline inside the wheel's reach was refused"
    );
    assert!(
        wheel.try_insert(start + reach, 2).is_none(),
        "a deadline at the wheel's reach was accepted"
    );
    assert!(
        wheel.try_insert(start + reach * 2, 3).is_none(),
        "a deadline well past the wheel's reach was accepted"
    );
    assert_eq!(wheel.len(), 1, "a refused insertion left something behind");
}
