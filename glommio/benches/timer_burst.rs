//! What it costs when a whole population of timers comes due at once.
//!
//! The cases in `timer.rs` deliberately park their cluster an hour out so it
//! never fires inside a measured window. That answers what a parked cluster
//! costs the timers running beside it, and leaves the opposite question open:
//! what the cluster costs when it actually arrives. A read timeout is a single
//! duration, so connections that arrive together expire together, and the
//! clustered slot is the normal case rather than the pathological one.
//!
//! Two things are worth separating there, and they are not the same number:
//!
//! - what the timers in the burst suffer, which is lateness against the
//!   deadline each one asked for, and
//! - what everything else on the executor suffers while the burst drains,
//!   which is the gap a co-running task sees between its turns.
//!
//! Percentiles rather than a mean, because the mean is what hides this: the
//! whole population is late and averaging over it reports a number no caller
//! experiences. That is also why this is not a criterion benchmark.
//!
//! Comparing against another implementation means running this same file from
//! that checkout; it uses no API the wheel introduced.

use glommio::{timer::sleep, LocalExecutor};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

const POPULATIONS: &[usize] = &[64, 256, 1_024, 4_096, 16_384];

/// Far enough out that spawning the population finishes before it comes due.
const CLUSTER: Duration = Duration::from_millis(200);

/// How long the co-running task keeps yielding, which has to outlast the burst.
const RUN_FOR: Duration = Duration::from_millis(400);

/// Lateness of every timer in a population that shares one deadline.
///
/// Each task targets the same instant rather than computing its own when it
/// happens to be polled first, so the population lands in one tick instead of
/// smearing across several and diluting the case being measured.
fn lateness(n: usize) -> Vec<Duration> {
    LocalExecutor::default().run(async move {
        let out = Rc::new(RefCell::new(Vec::with_capacity(n)));
        let target = Instant::now() + CLUSTER;
        let mut tasks = Vec::with_capacity(n);
        for _ in 0..n {
            let out = Rc::clone(&out);
            tasks.push(glommio::spawn_local(async move {
                let now = Instant::now();
                assert!(now < target, "spawning outran the cluster deadline");
                sleep(target - now).await;
                out.borrow_mut()
                    .push(Instant::now().saturating_duration_since(target));
            }));
        }
        for t in tasks {
            t.await;
        }
        Rc::try_unwrap(out).unwrap().into_inner()
    })
}

/// The longest a task that only yields went without a turn, while `n` timers
/// came due together. With `n` of zero this is the floor the rest is read
/// against.
fn worst_co_running_gap(n: usize) -> (Duration, u64) {
    LocalExecutor::default().run(async move {
        let done = Rc::new(Cell::new(false));
        let target = Instant::now() + CLUSTER;

        let mut timers = Vec::with_capacity(n);
        for _ in 0..n {
            timers.push(glommio::spawn_local(async move {
                sleep(target.saturating_duration_since(Instant::now())).await;
            }));
        }

        let victim = glommio::spawn_local({
            let done = Rc::clone(&done);
            async move {
                let (mut worst, mut laps) = (Duration::ZERO, 0u64);
                while !done.get() {
                    let t = Instant::now();
                    glommio::executor().yield_task_queue_now().await;
                    worst = worst.max(t.elapsed());
                    laps += 1;
                }
                (worst, laps)
            }
        });

        for t in timers {
            t.await;
        }
        sleep(RUN_FOR.saturating_sub(CLUSTER)).await;
        done.set(true);
        victim.await
    })
}

fn pct(sorted: &[Duration], p: f64) -> u128 {
    let i = (((sorted.len() - 1) as f64) * p).round() as usize;
    sorted[i].as_micros()
}

fn main() {
    println!("timer lateness when one deadline is shared, microseconds");
    println!(
        "{:>7}  {:>7} {:>7} {:>7} {:>7}",
        "n", "p50", "p99", "p99.9", "max"
    );
    for &n in POPULATIONS {
        let _ = lateness(n);
        let mut v = lateness(n);
        v.sort_unstable();
        println!(
            "{n:>7}  {:>7} {:>7} {:>7} {:>7}",
            pct(&v, 0.50),
            pct(&v, 0.99),
            pct(&v, 0.999),
            v.last().unwrap().as_micros()
        );
    }

    println!();
    println!("worst gap seen by a task doing nothing but yielding, microseconds");
    println!("{:>7}  {:>9} {:>10}", "n", "worst", "turns");
    for &n in std::iter::once(&0).chain(POPULATIONS) {
        let _ = worst_co_running_gap(n);
        let (worst, laps) = worst_co_running_gap(n);
        println!("{n:>7}  {:>9} {laps:>10}", worst.as_micros());
    }
}
