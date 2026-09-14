//! What the timer structure costs, in the shape glommio uses it.
//!
//! Instrumenting a server first said the workload is arming and cancelling
//! with almost no expiry: a read timeout is set per operation and withdrawn
//! when the read completes, so at 4096 connections every timer was cancelled
//! and none fired. These cases are weighted accordingly.
//!
//! Timed through `iter_custom` with the executor driven by hand rather than
//! `to_async`, because the measured window has to exclude building the timers
//! and dropping them, which are the other case in the same benchmark.

mod common;

use common::Glommio;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use futures::stream::{FuturesUnordered, StreamExt};
use futures_lite::future::poll_once;
use glommio::timer::{sleep, Timer};
use std::{
    hint::black_box,
    time::{Duration, Instant},
};

const POPULATIONS: &[usize] = &[64, 256, 1_024, 4_096];

/// Far past any population the other cases use.
///
/// The question these answer is not how the structure behaves at the sizes a
/// server reaches today but whether its curve ever turns up. A million timers
/// is eight more levels of a B-tree than four thousand, so if `O(log n)` is
/// going to cost anything it has to cost it here.
const PREMISE_POPULATIONS: &[usize] = &[64, 4_096, 65_536, 262_144, 1_048_576];

/// No small sizes, because the expiry cases divide one wake-up across the whole
/// population: at 64 timers the executor's single return from the kernel is the
/// entire measurement, and at 65,536 it is a rounding error.
const EXPIRING_POPULATIONS: &[usize] = &[4_096, 65_536, 262_144, 1_048_576];

/// Far enough out that nothing in these cases reaches it.
const PARKED: Duration = Duration::from_secs(3_600);

/// Registering a timer that will not fire.
fn arm(c: &mut Criterion) {
    let ex = Glommio::default();
    let mut group = c.benchmark_group("timer/arm");
    group.sample_size(10).warm_up_time(Duration::from_secs(1));

    for &n in PREMISE_POPULATIONS {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_custom(|iters| {
                ex.0.run(async {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let mut timers: Vec<Timer> = (0..n).map(|_| Timer::new(PARKED)).collect();

                        let started = Instant::now();
                        for timer in timers.iter_mut() {
                            // The first poll is what registers it.
                            black_box(poll_once(timer).await);
                        }
                        total += started.elapsed();
                    }
                    total / n as u32
                })
            })
        });
    }
    group.finish();
}

/// Withdrawing a timer that has not fired, which the measured workload does to
/// every timer it creates.
fn cancel(c: &mut Criterion) {
    let ex = Glommio::default();
    let mut group = c.benchmark_group("timer/cancel");
    group.sample_size(10).warm_up_time(Duration::from_secs(1));

    for &n in PREMISE_POPULATIONS {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_custom(|iters| {
                ex.0.run(async {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let mut timers: Vec<Timer> = (0..n).map(|_| Timer::new(PARKED)).collect();
                        for timer in timers.iter_mut() {
                            poll_once(timer).await;
                        }

                        let started = Instant::now();
                        drop(timers);
                        total += started.elapsed();
                    }
                    total / n as u32
                })
            })
        });
    }
    group.finish();
}

/// A short sleep with a population already waiting.
///
/// Two things show up here. A structure that answers "what is due next" by
/// scanning grows with the population. And one that reports the tick a
/// deadline was rounded into rather than the deadline itself puts a floor
/// under every sleep shorter than its resolution.
fn sleep_under_population(c: &mut Criterion) {
    let ex = Glommio::default();
    let mut group = c.benchmark_group("timer/sleep_100us");
    group.sample_size(10).warm_up_time(Duration::from_secs(1));

    for &n in PREMISE_POPULATIONS {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_custom(|iters| {
                ex.0.run(async {
                    let mut parked: Vec<Timer> = (0..n).map(|_| Timer::new(PARKED)).collect();
                    for timer in parked.iter_mut() {
                        poll_once(timer).await;
                    }

                    let started = Instant::now();
                    for _ in 0..iters {
                        sleep(Duration::from_micros(100)).await;
                    }
                    let elapsed = started.elapsed();

                    drop(parked);
                    elapsed
                })
            })
        });
    }
    group.finish();
}

/// A short sleep with a population whose deadlines all fall in one slot.
///
/// [`PARKED`] deadlines are an hour out, which is inside the wheel's reach:
/// they share one level-3 slot rather than reaching the overflow map, and no
/// case here builds a populated slot at a level the reactor reads on a poll. Timers armed for the
/// same near deadline all land together, which is what a burst of connections
/// sharing a read timeout produces. Answering "what is due next" by scanning
/// that slot grows with the population; the wheel is meant not to.
fn sleep_under_clustered_population(c: &mut Criterion) {
    // Inside the finest level, so the whole population shares one slot, and far
    // enough out that nothing fires while a batch is measured.
    const CLUSTER: Duration = Duration::from_millis(200);
    const SLEEPS_PER_BATCH: usize = 400;

    let ex = Glommio::default();
    let mut group = c.benchmark_group("timer/sleep_100us_clustered");

    for &n in POPULATIONS {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_custom(|iters| {
                ex.0.run(async {
                    let mut total = Duration::ZERO;
                    let mut remaining = iters;
                    while remaining > 0 {
                        let this_batch = remaining.min(SLEEPS_PER_BATCH as u64);
                        // Rebuilt per batch, outside the timed region, because
                        // the cluster would otherwise come due mid-measurement.
                        let mut cluster: Vec<Timer> = (0..n).map(|_| Timer::new(CLUSTER)).collect();
                        for timer in cluster.iter_mut() {
                            black_box(poll_once(timer).await);
                        }

                        let started = Instant::now();
                        for _ in 0..this_batch {
                            sleep(Duration::from_micros(100)).await;
                        }
                        total += started.elapsed();

                        drop(cluster);
                        remaining -= this_batch;
                    }
                    total
                })
            })
        });
    }
    group.finish();
}

/// A short sleep with a population clustered in a *coarse* level.
///
/// One second is past level 0, so the whole population shares one level-1
/// slot. Asking whether anything there has come due is what separates a slot
/// that answers from its cached minimum from one that is walked.
fn sleep_under_coarse_clustered_population(c: &mut Criterion) {
    const CLUSTER: Duration = Duration::from_secs(1);
    const SLEEPS_PER_BATCH: usize = 400;

    let ex = Glommio::default();
    let mut group = c.benchmark_group("timer/sleep_100us_coarse");

    for &n in POPULATIONS {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_custom(|iters| {
                ex.0.run(async {
                    let mut total = Duration::ZERO;
                    let mut remaining = iters;
                    while remaining > 0 {
                        let this_batch = remaining.min(SLEEPS_PER_BATCH as u64);
                        let mut cluster: Vec<Timer> = (0..n).map(|_| Timer::new(CLUSTER)).collect();
                        for timer in cluster.iter_mut() {
                            black_box(poll_once(timer).await);
                        }

                        let started = Instant::now();
                        for _ in 0..this_batch {
                            sleep(Duration::from_micros(100)).await;
                        }
                        total += started.elapsed();

                        drop(cluster);
                        remaining -= this_batch;
                    }
                    total
                })
            })
        });
    }
    group.finish();
}

/// A whole population coming due, which nothing else here measures.
///
/// Every other case parks its timers an hour out so none of them fire, which
/// leaves the structure's expiry path unmeasured on both sides. Main splits its
/// map and drains the prefix; the wheel takes a slot whole. This is also the
/// only case that exercises waking, so the per-timer figure includes the waker
/// call that both structures have to make.
///
/// Every timer is given the same absolute deadline, so the whole population
/// comes due at one instant and every bit of the work lands after it. The
/// population is armed outside the timed region and the idle wait is subtracted
/// exactly, which leaves the firing and nothing else. Staggering the deadlines
/// instead would spread most of the work across the wait being subtracted.
fn expire(c: &mut Criterion) {
    // Inside the finest level, so the population shares one slot and nothing
    // cascades on the way to firing.
    const WAIT: Duration = Duration::from_millis(250);

    let ex = Glommio::default();
    let mut group = c.benchmark_group("timer/expire");
    group.sample_size(10).warm_up_time(Duration::from_secs(1));

    // A level-0 deadline has to be under 256ms, and the population has to be
    // armed before it arrives, which is what bounds this case rather than
    // anything structural.
    for &n in EXPIRING_POPULATIONS.iter().filter(|n| **n <= 262_144) {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_custom(|iters| {
                ex.0.run(async {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        total += fire_all(n, WAIT).await;
                    }
                    total / n as u32
                })
            })
        });
    }
    group.finish();
}

/// The same population, placed one level out so that it has to cascade first.
///
/// One second is past level 0, so every timer is filed in a level-1 slot and
/// the whole slot is broken down into level 0 before any of it can fire. Main
/// has no equivalent step, so what this costs over [`expire`] is what the
/// hierarchy charges to maintain itself.
fn expire_after_cascade(c: &mut Criterion) {
    const WAIT: Duration = Duration::from_secs(1);

    let ex = Glommio::default();
    let mut group = c.benchmark_group("timer/expire_cascaded");
    group.sample_size(10).warm_up_time(Duration::from_secs(1));

    for &n in EXPIRING_POPULATIONS {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_custom(|iters| {
                ex.0.run(async {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        total += fire_all(n, WAIT).await;
                    }
                    total / n as u32
                })
            })
        });
    }
    group.finish();
}

/// Arms `n` timers at `wait`, waits for all of them, and returns the time spent
/// on everything but the waiting.
async fn fire_all(n: usize, wait: Duration) -> Duration {
    // `Timer::new` is relative and reads the clock itself, so shrinking the
    // duration as the loop runs is what gives the whole population one shared
    // deadline.
    let target = Instant::now() + wait;
    let mut timers: Vec<Timer> = (0..n)
        .map(|_| Timer::new(target.saturating_duration_since(Instant::now())))
        .collect();
    // Registering is what the first poll does, and it is measured by `arm`.
    for timer in timers.iter_mut() {
        black_box(poll_once(timer).await);
    }

    let started = Instant::now();
    assert!(
        started < target,
        "arming {n} timers outlasted the {wait:?} wait, so they were already due"
    );
    let mut pending: FuturesUnordered<Timer> = timers.into_iter().collect();
    while pending.next().await.is_some() {}
    started.elapsed() - target.duration_since(started)
}

/// A sleep short enough that answering "what is due next" is most of it.
///
/// Both structures are asked for their earliest deadline on every park: main
/// walks to the leftmost key of its map, the wheel scans an occupancy mask and
/// reads one slot. Neither has ever been measured, because the only case that
/// reaches the question sleeps 100us and buries it.
///
/// A microsecond does not bury it. What is left is one sleep's whole cost with
/// a population present: arming the sleep, being asked for the next deadline,
/// and firing. Those are separately measured by [`arm`] and [`expire`], so
/// growth here beyond what those account for belongs to the question itself.
///
/// The population is parked an hour out, so it never becomes the answer and
/// never fires; it is there to be searched past.
fn short_sleep_under_population(c: &mut Criterion) {
    let ex = Glommio::default();
    let mut group = c.benchmark_group("timer/sleep_1us");
    group.sample_size(10).warm_up_time(Duration::from_secs(1));

    for &n in PREMISE_POPULATIONS {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_custom(|iters| {
                ex.0.run(async {
                    let mut parked: Vec<Timer> = (0..n).map(|_| Timer::new(PARKED)).collect();
                    for timer in parked.iter_mut() {
                        black_box(poll_once(timer).await);
                    }

                    let started = Instant::now();
                    for _ in 0..iters {
                        sleep(Duration::from_micros(1)).await;
                    }
                    let elapsed = started.elapsed();

                    drop(parked);
                    elapsed
                })
            })
        });
    }
    group.finish();
}

/// Waking after a long idle stretch, which is the one case the wheel could
/// lose.
///
/// Main reads the first key of its map and is done, whatever the gap. The wheel
/// has to move its cursor from where it was to where it is, and a naive one
/// would pay per millisecond crossed: ten idle minutes with a populated wheel
/// would cost six hundred thousand iterations for time in which nothing was
/// due. `advance_to` steps to the next tick that has work instead, so the cost
/// is meant to be the work and not the interval. This is what says whether that
/// holds.
///
/// The gap has to be inside the timed region, because the crossing happens when
/// the sleep completes, so the known gap is subtracted and what remains is the
/// wake plus the crossing. A 1us sleep puts the wake floor near 6us, so a cost
/// proportional to the gap would be unmissable: 100ms is a hundred thousand
/// ticks, and even a nanosecond each would be 100us.
///
/// The population sits ten seconds out, which is a level-1 slot, so the wheel
/// has a coarse level to cascade and cannot skip the gap in one jump.
fn idle_gap(c: &mut Criterion) {
    const GAPS_MS: &[u64] = &[1, 10, 100];
    const POPULATION: Duration = Duration::from_secs(10);

    let ex = Glommio::default();
    let mut group = c.benchmark_group("timer/idle_gap");
    group.sample_size(10).warm_up_time(Duration::from_secs(1));

    for &n in &[4_096usize, 262_144] {
        for &gap_ms in GAPS_MS {
            group.bench_with_input(
                BenchmarkId::new(format!("{n}"), format!("{gap_ms}ms")),
                &(n, gap_ms),
                |b, &(n, gap_ms)| {
                    b.iter_custom(|iters| {
                        ex.0.run(async {
                            let gap = Duration::from_millis(gap_ms);
                            let mut parked: Vec<Timer> =
                                (0..n).map(|_| Timer::new(POPULATION)).collect();
                            for timer in parked.iter_mut() {
                                black_box(poll_once(timer).await);
                            }

                            let mut total = Duration::ZERO;
                            for _ in 0..iters {
                                let started = Instant::now();
                                sleep(gap).await;
                                total += started.elapsed().saturating_sub(gap);
                            }

                            drop(parked);
                            total
                        })
                    })
                },
            );
        }
    }
    group.finish();
}

criterion_group!(
    benches,
    arm,
    cancel,
    idle_gap,
    short_sleep_under_population,
    expire,
    expire_after_cascade,
    sleep_under_population,
    sleep_under_clustered_population,
    sleep_under_coarse_clustered_population
);
criterion_main!(benches);
