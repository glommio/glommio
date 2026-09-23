// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience
//
//! What the wheel costs, and whether making it configurable cost anything.
//!
//! The layout is a runtime value rather than a set of constants, so that a
//! caller can choose it. The claim being tested is that this is free: slot
//! counts and resolutions are powers of two held as a shift and a mask, so the
//! arithmetic a constant layout would compile to is the arithmetic this does.
//!
//! What matters here is the shape of each curve, not the absolute figure. A
//! cost that stays flat from sixty-four entries to a million is the property
//! the structure exists to provide; the integrated numbers only mean something
//! once a real caller is driving it.

use criterion::{
    criterion_group, criterion_main, measurement::WallTime, BenchmarkGroup, BenchmarkId, Criterion,
};
use glommio::timer::wheel::{CoarseQuery, Key, Millis, Overflow, Policy, Rounding, TimingWheel};
use std::marker::PhantomData;
use std::{
    hint::black_box,
    time::{Duration, Instant},
};

const POPULATIONS: &[usize] = &[64, 4_096, 65_536, 1_048_576];

/// Far enough out that nothing in these cases comes due.
const PARKED: Duration = Duration::from_secs(3_600);

/// The same layout, answering the next deadline exactly rather than by the
/// bucket it sits in.
struct Exact;
impl Policy for Exact {
    const SLOTS: &'static [usize] = Millis::SLOTS;
    const RESOLUTION: &'static [u64] = Millis::RESOLUTION;
    const COARSE_QUERY: CoarseQuery = CoarseQuery::ExactMinimum;
}

fn filled<P: Policy>(start: Instant, n: usize) -> (TimingWheel<usize, P>, Vec<Key>) {
    let mut wheel = TimingWheel::starting_at(start);
    let keys = (0..n)
        .map(|i| {
            // Spread within the slot so entries are not all one deadline.
            wheel.insert(start + PARKED + Duration::from_nanos(i as u64), i)
        })
        .collect();
    (wheel, keys)
}

fn insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert");
    for &n in POPULATIONS {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let start = Instant::now();
                    let mut wheel: TimingWheel<usize, Millis> = TimingWheel::starting_at(start);
                    let began = Instant::now();
                    for i in 0..n {
                        black_box(wheel.insert(start + PARKED + Duration::from_nanos(i as u64), i));
                    }
                    total += began.elapsed();
                }
                total / n as u32
            })
        });
    }
    group.finish();
}

/// Withdrawing earliest-first, which is what a caller arming a timeout per
/// operation and cancelling it on completion produces, and the order that
/// costs a tracked minimum the most.
fn cancel_earliest_first(c: &mut Criterion) {
    let mut group = c.benchmark_group("cancel_earliest_first");
    for &n in POPULATIONS {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let (mut wheel, keys) = filled::<Millis>(Instant::now(), n);
                    let began = Instant::now();
                    for key in keys {
                        black_box(wheel.remove(key));
                    }
                    total += began.elapsed();
                }
                total / n as u32
            })
        });
    }
    group.finish();
}

/// Asking what is due next, in isolation.
///
/// A caller asks this before every sleep, and inside a runtime it is
/// impossible to separate from arming and firing. Here it is the only thing
/// happening, which is the whole reason to measure it in this crate.
fn next_expiry(c: &mut Criterion) {
    let mut group = c.benchmark_group("next_expiry");
    for &n in POPULATIONS {
        group.bench_with_input(BenchmarkId::new("bucket_bound", n), &n, |b, &n| {
            let (wheel, _keys) = filled::<Millis>(Instant::now(), n);
            b.iter(|| black_box(wheel.next_expiry()))
        });
        group.bench_with_input(BenchmarkId::new("exact_minimum", n), &n, |b, &n| {
            let (wheel, _keys) = filled::<Exact>(Instant::now(), n);
            b.iter(|| black_box(wheel.next_expiry()))
        });
    }
    group.finish();
}

/// Waking after a long idle stretch.
///
/// A wheel that steps every tick would pay per millisecond crossed. This is
/// meant to pay for the work instead, so the curve should be flat in the gap.
fn idle_gap(c: &mut Criterion) {
    let mut group = c.benchmark_group("idle_gap");
    for &gap_ms in &[1u64, 1_000, 60_000, 600_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(gap_ms),
            &gap_ms,
            |b, &gap_ms| {
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let start = Instant::now();
                        let (mut wheel, _keys) = filled::<Millis>(start, 4_096);
                        let began = Instant::now();
                        wheel.advance_to(start + Duration::from_millis(gap_ms));
                        total += began.elapsed();
                    }
                    total
                })
            },
        );
    }
    group.finish();
}

/// Cancelling earliest-first while asking what is due next, which is the case
/// the two coarse-query policies exist to trade between.
///
/// Neither `cancel_earliest_first` nor `next_expiry` separates them: one never
/// asks, and the other never disturbs the answer. A caller that arms a timeout
/// per operation and cancels it on completion does both, continuously, and
/// withdrawing the entry that supplied a slot's minimum is what invalidates it.
fn churn<P: Policy>(group: &mut BenchmarkGroup<'_, WallTime>, label: &str) {
    // Deliberately smaller than the other cases. Every entry here shares one
    // slot, so a policy that rescans it on each query is quadratic, and at a
    // million entries this case does not finish. That it does not finish is
    // the finding; the sizes below are enough to show the shape.
    for &n in &[64usize, 512, 4_096] {
        group.bench_with_input(BenchmarkId::new(label, n), &n, |b, &n| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let (mut wheel, keys) = filled::<P>(Instant::now(), n);
                    let began = Instant::now();
                    for key in keys {
                        black_box(wheel.remove(key));
                        black_box(wheel.next_expiry());
                    }
                    total += began.elapsed();
                }
                total / n as u32
            })
        });
    }
}

fn cancel_with_query(c: &mut Criterion) {
    let mut group = c.benchmark_group("cancel_with_query");
    churn::<Millis>(&mut group, "bucket_bound");
    churn::<Exact>(&mut group, "exact_minimum");
    group.finish();
}

// ---- the matrix ---------------------------------------------------------
//
// Six layouts and five policy choices is seventy-two combinations, and
// measuring all of them at several populations would run for hours. Most axes
// should not affect cost at all, so each is varied on its own: the question
// for rounding and overflow is whether they are free, and a sweep answers that
// as well as a cross product would.

trait Layout {
    const NAME: &'static str;
    const SLOTS: &'static [usize];
    const RESOLUTION: &'static [u64];
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
    Flat:      [64], [1];
    Deep:      [2, 2, 2, 2, 2, 2, 2, 2], [1, 2, 4, 8, 16, 32, 64, 128];
    Wide:      [1024, 1024], [1, 1024];
    Uneven:    [256, 8, 64, 4], [1, 256, 2048, 131_072];
    Realistic: [256, 64, 64, 64], [1, 256, 16_384, 1_048_576];
}

struct Shaped<L>(PhantomData<L>);

impl<L: Layout> Policy for Shaped<L> {
    const SLOTS: &'static [usize] = L::SLOTS;
    const RESOLUTION: &'static [u64] = L::RESOLUTION;
}

struct Rounded<R>(PhantomData<R>);

trait Round {
    const NAME: &'static str;
    const VALUE: Rounding;
}

macro_rules! roundings {
    ($($name:ident => $value:expr;)*) => {$(
        struct $name;
        impl Round for $name {
            const NAME: &'static str = stringify!($name);
            const VALUE: Rounding = $value;
        }
    )*};
}

roundings! {
    Up => Rounding::Up;
    Down => Rounding::Down;
    Nearest => Rounding::Nearest;
}

impl<R: Round> Policy for Rounded<R> {
    const SLOTS: &'static [usize] = Millis::SLOTS;
    const RESOLUTION: &'static [u64] = Millis::RESOLUTION;
    const ROUNDING: Rounding = R::VALUE;
}

struct Rejecting;
impl Policy for Rejecting {
    const SLOTS: &'static [usize] = Millis::SLOTS;
    const RESOLUTION: &'static [u64] = Millis::RESOLUTION;
    const OVERFLOW: Overflow = Overflow::Reject;
}

/// A deadline comfortably inside a layout's reach, whatever that reach is.
fn inside<P: Policy>(i: usize) -> Duration {
    let reach = P::RESOLUTION[P::RESOLUTION.len() - 1] * P::SLOTS[P::SLOTS.len() - 1] as u64;
    Duration::from_micros((reach * 1_000 / 2) + i as u64)
}

fn layout_ops<L: Layout>(group: &mut BenchmarkGroup<'_, WallTime>, n: usize) {
    type P<L> = Shaped<L>;
    group.bench_function(BenchmarkId::new("insert", L::NAME), |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let start = Instant::now();
                let mut wheel: TimingWheel<usize, P<L>> = TimingWheel::starting_at(start);
                let began = Instant::now();
                for i in 0..n {
                    black_box(wheel.insert(start + inside::<P<L>>(i), i));
                }
                total += began.elapsed();
            }
            total / n as u32
        })
    });
    group.bench_function(BenchmarkId::new("cancel", L::NAME), |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let start = Instant::now();
                let mut wheel: TimingWheel<usize, P<L>> = TimingWheel::starting_at(start);
                let keys: Vec<_> = (0..n)
                    .map(|i| wheel.insert(start + inside::<P<L>>(i), i))
                    .collect();
                let began = Instant::now();
                for key in keys {
                    black_box(wheel.remove(key));
                }
                total += began.elapsed();
            }
            total / n as u32
        })
    });
    group.bench_function(BenchmarkId::new("next_expiry", L::NAME), |b| {
        let start = Instant::now();
        let mut wheel: TimingWheel<usize, P<L>> = TimingWheel::starting_at(start);
        for i in 0..n {
            wheel.insert(start + inside::<P<L>>(i), i);
        }
        b.iter(|| black_box(wheel.next_expiry()))
    });
}

/// Does the shape of the wheel cost anything?
fn by_layout(c: &mut Criterion) {
    let mut group = c.benchmark_group("by_layout");
    layout_ops::<Flat>(&mut group, 4_096);
    layout_ops::<Deep>(&mut group, 4_096);
    layout_ops::<Wide>(&mut group, 4_096);
    layout_ops::<Uneven>(&mut group, 4_096);
    layout_ops::<Realistic>(&mut group, 4_096);
    group.finish();
}

/// Rounding is one match on a constant, so it should be free. This is the
/// case that says whether it is.
fn by_rounding(c: &mut Criterion) {
    let mut group = c.benchmark_group("by_rounding");
    fn one<R: Round>(group: &mut BenchmarkGroup<'_, WallTime>) {
        group.bench_function(BenchmarkId::new("insert", R::NAME), |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let start = Instant::now();
                    let mut wheel: TimingWheel<usize, Rounded<R>> = TimingWheel::starting_at(start);
                    let began = Instant::now();
                    for i in 0..4_096 {
                        black_box(wheel.insert(start + PARKED + Duration::from_nanos(i as u64), i));
                    }
                    total += began.elapsed();
                }
                total / 4_096
            })
        });
    }
    one::<Up>(&mut group);
    one::<Down>(&mut group);
    one::<Nearest>(&mut group);
    group.finish();
}

/// Refusing what the wheel cannot hold adds a check to every insertion.
fn by_overflow(c: &mut Criterion) {
    let mut group = c.benchmark_group("by_overflow");
    for (label, park) in [("park", true), ("reject", false)] {
        group.bench_function(label, |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let start = Instant::now();
                    let began;
                    if park {
                        let mut wheel: TimingWheel<usize, Millis> = TimingWheel::starting_at(start);
                        began = Instant::now();
                        for i in 0..4_096 {
                            black_box(
                                wheel.insert(start + PARKED + Duration::from_nanos(i as u64), i),
                            );
                        }
                    } else {
                        let mut wheel: TimingWheel<usize, Rejecting> =
                            TimingWheel::starting_at(start);
                        began = Instant::now();
                        for i in 0..4_096 {
                            black_box(
                                wheel.insert(start + PARKED + Duration::from_nanos(i as u64), i),
                            );
                        }
                    }
                    total += began.elapsed();
                }
                total / 4_096
            })
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    by_layout,
    by_rounding,
    by_overflow,
    insert,
    cancel_earliest_first,
    next_expiry,
    cancel_with_query,
    idle_gap
);
criterion_main!(benches);
